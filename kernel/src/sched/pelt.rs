use core::intrinsics::unlikely;

use alloc::sync::Arc;

use crate::process::ProcessControlBlock;

use super::{
    fair::{CfsRunQueue, FairSchedEntity},
    CpuRunQueue, LoadWeight, SchedClass, SCHED_CAPACITY_SCALE, SCHED_CAPACITY_SHIFT,
};

const RUNNABLE_AVG_Y_N_INV: [u32; 32] = [
    0xffffffff, 0xfa83b2da, 0xf5257d14, 0xefe4b99a, 0xeac0c6e6, 0xe5b906e6, 0xe0ccdeeb, 0xdbfbb796,
    0xd744fcc9, 0xd2a81d91, 0xce248c14, 0xc9b9bd85, 0xc5672a10, 0xc12c4cc9, 0xbd08a39e, 0xb8fbaf46,
    0xb504f333, 0xb123f581, 0xad583ee9, 0xa9a15ab4, 0xa5fed6a9, 0xa2704302, 0x9ef5325f, 0x9b8d39b9,
    0x9837f050, 0x94f4efa8, 0x91c3d373, 0x8ea4398a, 0x8b95c1e3, 0x88980e80, 0x85aac367, 0x82cd8698,
];

pub const LOAD_AVG_PERIOD: u64 = 32;
pub const LOAD_AVG_MAX: usize = 47742;
pub const PELT_MIN_DIVIDER: usize = LOAD_AVG_MAX - 1024;

#[derive(Debug, Default)]
pub struct SchedulerAvg {
    /// 存储上次更新这些平均值的时间
    pub last_update_time: u64,
    /// 存储所有可运行任务的负载之和
    pub load_sum: u64,
    /// 存储所有可运行任务的时间之和
    pub runnable_sum: u64,
    /// 存储所有运行任务的时间之和
    pub util_sum: u64,
    /// 记录周期性任务的贡献值，用于计算平均CPU利用率
    pub period_contrib: u32,

    pub load_avg: usize,
    pub runnable_avg: usize,
    pub util_avg: usize,
}

impl SchedulerAvg {
    #[inline]
    pub fn get_pelt_divider(&self) -> usize {
        return PELT_MIN_DIVIDER + self.period_contrib as usize;
    }

    pub fn update_load_sum(
        &mut self,
        now: u64,
        load: u64,
        mut runnable: u64,
        mut running: u32,
    ) -> bool {
        if now < self.last_update_time {
            self.last_update_time = now;
            return false;
        }

        let mut delta = now - self.last_update_time;
        delta >>= 10;

        if delta == 0 {
            return false;
        }

        self.last_update_time += delta << 10;

        if load == 0 {
            runnable = 0;
            running = 0;
        }

        self.accumulate_sum(delta, load, runnable, running) != 0
    }

    pub fn accumulate_sum(
        &mut self,
        mut delta: u64,
        load: u64,
        runnable: u64,
        running: u32,
    ) -> u64 {
        let mut contrib = delta as u32;

        delta += self.period_contrib as u64;

        let periods = delta / 1024;

        if periods > 0 {
            self.load_sum = Self::decay_load(self.load_sum, periods);
            self.runnable_sum = Self::decay_load(self.runnable_sum, periods);
            self.util_sum = Self::decay_load(self.util_sum, periods);

            delta %= 1024;
            if load > 0 {
                contrib = Self::accumulate_pelt_segments(
                    periods,
                    1024 - self.period_contrib,
                    delta as u32,
                );
            }
        }

        self.period_contrib = delta as u32;

        if load > 0 {
            self.load_sum += contrib as u64 * load;
        }
        if runnable > 0 {
            self.runnable_sum += (runnable * contrib as u64) << SCHED_CAPACITY_SHIFT;
        }

        if running > 0 {
            self.util_sum += (contrib << SCHED_CAPACITY_SHIFT) as u64;
        }

        return periods;
    }

    fn decay_load(mut val: u64, n: u64) -> u64 {
        // y^32 ~= 1/2. Only discard history beyond 63 half-lives;
        // stopping after the first half-life erases still-significant load.
        if unlikely(n > LOAD_AVG_PERIOD * 63) {
            return 0;
        }

        let mut local_n = n;

        if unlikely(local_n >= LOAD_AVG_PERIOD) {
            val >>= local_n / LOAD_AVG_PERIOD;
            local_n %= LOAD_AVG_PERIOD;
        }

        ((val as i128 * RUNNABLE_AVG_Y_N_INV[local_n as usize] as i128) >> 32) as u64
    }

    fn accumulate_pelt_segments(periods: u64, d1: u32, d3: u32) -> u32 {
        /* y^0 == 1 */
        let c3 = d3;

        /*
         * c1 = d1 y^p
         */
        let c1 = Self::decay_load(d1 as u64, periods) as u32;

        /*
         *            p-1
         * c2 = 1024 \Sum y^n
         *            n=1
         *
         *              inf        inf
         *    = 1024 ( \Sum y^n - \Sum y^n - y^0 )
         *              n=0        n=p
         */
        let c2 = LOAD_AVG_MAX as u32 - Self::decay_load(LOAD_AVG_MAX as u64, periods) as u32 - 1024;

        return c1 + c2 + c3;
    }

    pub fn update_load_avg(&mut self, load: u64) {
        let divider = self.get_pelt_divider();

        self.load_avg = (load * self.load_sum) as usize / divider;
        self.runnable_avg = self.runnable_sum as usize / divider;
        self.util_avg = self.util_sum as usize / divider;
    }

    #[allow(dead_code)]
    pub fn post_init_entity_util_avg(pcb: &Arc<ProcessControlBlock>) {
        let se = pcb.sched_info().sched_entity();
        let cfs_rq = se.cfs_rq();
        let sa = &mut se.force_mut().avg;

        // TODO: 这里和架构相关
        let cpu_scale = SCHED_CAPACITY_SCALE;

        let cap = (cpu_scale as isize - cfs_rq.avg.util_avg as isize) / 2;

        if pcb.sched_info().sched_class() != SchedClass::Fair {
            sa.last_update_time = cfs_rq.cfs_rq_clock_pelt();
        }

        if cap > 0 {
            if cfs_rq.avg.util_avg != 0 {
                sa.util_avg = cfs_rq.avg.util_avg * se.load.weight as usize;
                sa.util_avg /= cfs_rq.avg.load_avg + 1;

                if sa.util_avg as isize > cap {
                    sa.util_avg = cap as usize;
                }
            } else {
                sa.util_avg = cap as usize;
            }
        }

        sa.runnable_avg = sa.util_avg;
    }
}

impl CpuRunQueue {
    pub fn rq_clock_pelt(&self) -> u64 {
        self.clock_pelt - self.lost_idle_time
    }
}

impl CfsRunQueue {
    pub fn cfs_rq_clock_pelt(&self) -> u64 {
        if unlikely(self.throttled_count > 0) {
            return self.throttled_clock_pelt - self.throttled_clock_pelt_time;
        }

        let rq = self.rq();
        let rq = rq.force_mut_locked();

        return rq.rq_clock_pelt() - self.throttled_clock_pelt_time;
    }
}

impl FairSchedEntity {
    pub fn update_load_avg(&mut self, cfs_rq: &mut CfsRunQueue, now: u64) -> bool {
        if self.avg.update_load_sum(
            now,
            self.on_rq() as u64,
            self.runnable(),
            cfs_rq.is_curr(&self.self_arc()) as u32,
        ) {
            self.avg
                .update_load_avg(LoadWeight::scale_load_down(self.load.weight));

            return true;
        }

        return false;
    }
}

bitflags! {
    pub struct UpdateAvgFlags: u8 {
        /// 更新任务组（task group）信息
        const UPDATE_TG	= 0x1;

        /// 跳过年龄和负载的更新
        const SKIP_AGE_LOAD	= 0x2;
        /// 执行附加操作
        const DO_ATTACH	= 0x4;
        /// 执行分离操作
        const DO_DETACH	= 0x8;
    }
}

pub fn add_positive(x: &mut isize, y: isize) {
    let res = *x + y;
    *x = res.max(0);
}

pub fn sub_positive(x: &mut usize, y: usize) {
    if *x > y {
        *x -= y;
    } else {
        *x = 0;
    }
}

/// Exercise the production PELT arithmetic without touching a live runqueue.
pub(crate) fn run_selftests() {
    let entity = FairSchedEntity::new();
    let mut queue = CfsRunQueue::new();
    let se = entity.force_mut();
    se.on_rq = super::OnRq::Queued;
    assert!(se.on_rq());
    assert_eq!(se.runnable(), 1);
    assert!(se.update_load_avg(&mut queue, 64 * 1024 * 1024));
    let load = se.avg.load_sum;
    let runnable = se.avg.runnable_sum;
    assert!(load != 0 && runnable != 0);
    se.on_rq = super::OnRq::None;
    assert!(!se.on_rq());
    assert_eq!(se.runnable(), 0);
    assert!(se.update_load_avg(&mut queue, 128 * 1024 * 1024));
    assert!(se.avg.load_sum < load && se.avg.runnable_sum < runnable);

    let mut heavy = SchedulerAvg::default();
    let mut unit = SchedulerAvg::default();
    heavy.accumulate_sum(65536, 262144, 1, 0);
    unit.accumulate_sum(65536, 1, 1, 0);
    assert_eq!(heavy.load_sum, unit.load_sum * 262144);
    assert!(heavy.load_sum > u32::MAX as u64);
    for delta in [511, 2048] {
        let mut one = SchedulerAvg::default();
        let mut three = SchedulerAvg::default();
        one.accumulate_sum(delta, 1, 1, 1);
        three.accumulate_sum(delta, 1, 3, 1);
        assert!(
            one.runnable_sum != 0,
            "PELT must accumulate runnable history"
        );
        assert_eq!(three.runnable_sum, one.runnable_sum * 3);
        assert_eq!(one.runnable_sum, one.load_sum << SCHED_CAPACITY_SHIFT);
    }

    let value = 1u64 << 40;
    let half = SchedulerAvg::decay_load(value, 32);
    let next = SchedulerAvg::decay_load(value, 33);
    let quarter = SchedulerAvg::decay_load(value, 64);
    assert!((value / 2 - 1024..=value / 2).contains(&half));
    assert!(
        next < half && next > value / 3,
        "PELT must retain history after 32 periods"
    );
    assert!((value / 4 - 1024..=value / 4).contains(&quarter));
    assert_eq!(
        SchedulerAvg::decay_load(u64::MAX, LOAD_AVG_PERIOD * 63 + 1),
        0
    );
}
