use core::intrinsics::unlikely;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::libs::rbtree::RBTree;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::new_sched::{SchedFeature, SCHED_FEATURES};
use alloc::sync::{Arc, Weak};

use super::{CpuRunQueue, LoadWeight, Scheduler};

/// 用于设置 CPU-bound 任务的最小抢占粒度的参数。
/// 默认值为 0.75 毫秒乘以（1 加上 CPU 数量的二进制对数），单位为纳秒。
/// 这个值影响到任务在 CPU-bound 情况下的抢占行为。
static SYSCTL_SHCED_MIN_GRANULARITY: AtomicU64 = AtomicU64::new(750000);
/// 规范化最小抢占粒度参数
static NORMALIZED_SYSCTL_SCHED_MIN_GRANULARITY: AtomicU64 = AtomicU64::new(750000);

/// 预设的调度延迟任务数量
static SCHED_NR_LATENCY: AtomicU64 = AtomicU64::new(8);

/// 调度实体单位，一个调度实体可以是一个进程、一个进程组或者是一个用户等等划分
pub struct FairSchedEntity {
    /// 负载相关
    pub load: LoadWeight,

    /// 是否在运行队列中
    pub on_rq: bool,
    /// 当前调度实体的开始执行时间
    pub exec_start: u64,
    /// 总运行时长
    pub sum_exec_runtime: u64,
    /// 虚拟运行时间
    pub vruntime: u64,
    /// 上一个调度实体运行总时间
    pub prev_sum_exec_runtime: u64,

    /// 父节点
    parent: Weak<FairSchedEntity>,

    /// 所在的CFS运行队列
    cfs_rq: &'static mut CompletelyFairScheduler,
}

impl FairSchedEntity {
    pub fn parent(&self) -> Option<Arc<FairSchedEntity>> {
        self.parent.upgrade()
    }

    pub fn force_mut(&self) -> &mut Self {
        unsafe { &mut *(self as *const Self as usize as *mut Self) }
    }

    /// 判断是否是进程持有的调度实体
    pub fn is_task(&self) -> bool {
        // TODO: 调度组
        true
    }

    pub fn calculate_delta_fair(&self, delta: u64) -> u64 {
        if unlikely(self.load.weight != LoadWeight::NICE_0_LOAD_SHIFT as u64) {
            return self
                .force_mut()
                .load
                .calculate_delta(delta, LoadWeight::NICE_0_LOAD_SHIFT as u64);
        };

        delta
    }
}

/// CFS的运行队列，这个队列是percpu的
pub struct CompletelyFairScheduler {
    load: LoadWeight,

    /// 全局运行的调度实体计数器，用于负载均衡
    nr_running: u64,
    /// 针对特定 CPU 核心的任务计数器
    self_nr_running: u64,
    /// 运行时间
    exec_clock: u64,
    /// 最少虚拟运行时间
    min_vruntime: u64,
    /// 存放调度实体的红黑树
    entities: RBTree<u64, Arc<FairSchedEntity>>,

    /// 当前运行的调度实体
    current: Weak<FairSchedEntity>,
    /// 下一个调度的实体
    next: Weak<FairSchedEntity>,
    /// 最后的调度实体
    last: Weak<FairSchedEntity>,
    /// 跳过运行的调度实体
    skip: Weak<FairSchedEntity>,

    rq: &'static mut CpuRunQueue,
}

impl CompletelyFairScheduler {
    pub fn current(&self) -> Option<Arc<FairSchedEntity>> {
        self.current.upgrade()
    }

    #[inline]
    pub const fn bandwidth_used() -> bool {
        false
    }

    /// ## 计算调度周期，基本思想是在一个周期内让每个任务都至少运行一次。
    /// 这样可以确保所有的任务都能够得到执行，而且可以避免某些任务被长时间地阻塞。
    pub fn sched_period(nr_running: u64) -> u64 {
        if unlikely(nr_running > SCHED_NR_LATENCY.load(Ordering::SeqCst)) {
            // 如果当前活跃的任务数量超过了预设的调度延迟任务数量
            // 调度周期的长度将直接设置为活跃任务数量乘以最小抢占粒度
            return nr_running * SYSCTL_SHCED_MIN_GRANULARITY.load(Ordering::SeqCst);
        } else {
            // 如果活跃任务数量未超过预设的延迟任务数量，那么调度周期的长度将设置为SCHED_NR_LATENCY
            return SCHED_NR_LATENCY.load(Ordering::SeqCst);
        }
    }

    /// ## 计算调度任务的虚拟运行时间片大小
    ///
    /// vruntime = runtime / weight
    pub fn sched_vslice(&self, entity: Arc<FairSchedEntity>) -> u64 {
        let slice = self.sched_slice(entity.clone());
        return entity.calculate_delta_fair(slice);
    }

    /// ## 计算调度任务的实际运行时间片大小
    pub fn sched_slice(&self, entity: Arc<FairSchedEntity>) -> u64 {
        let mut nr_running = self.nr_running;
        if SCHED_FEATURES.contains(SchedFeature::ALT_PERIOD) {
            nr_running = self.self_nr_running;
        }

        // 计算一个调度周期的整个slice
        let mut slice = Self::sched_period(nr_running + (!entity.on_rq) as u64);

        let mut se: Arc<FairSchedEntity> = entity;

        // 这一步是循环计算,直到根节点
        // 比如有任务组 A ，有进程B，B属于A任务组，那么B的时间分配依赖于A组的权重以及B进程自己的权重
        loop {
            let entity = se.force_mut();

            if unlikely(!entity.on_rq) {
                entity.cfs_rq.load.update_add(entity.load.weight);
            }

            slice = entity
                .cfs_rq
                .load
                .calculate_delta(slice, entity.load.weight);

            let parent = entity.parent();
            if parent.is_none() {
                break;
            }

            se = parent.unwrap();
        }

        if SCHED_FEATURES.contains(SchedFeature::BASE_SLICE) {
            // TODO: IDLE？
            let min_gran = SYSCTL_SHCED_MIN_GRANULARITY.load(Ordering::SeqCst);

            slice = min_gran.max(slice)
        }

        slice
    }

    /// ## 在时间片到期时检查当前任务是否需要被抢占，
    /// 如果需要，则抢占当前任务，并确保不会由于与其他任务的“好友偏爱（buddy favours）”而重新选举为下一个运行的任务。
    pub fn check_preempt_tick(&self, curr: Arc<FairSchedEntity>) {
        // 计算理想状态下该调度实体的理想运行时间
        let ideal_runtime = self.sched_slice(curr.clone());

        let delta_exec = curr.sum_exec_runtime - curr.prev_sum_exec_runtime;

        if delta_exec > ideal_runtime {
            // 表明实际运行时间长于理想运行时间
            self.rq.
        }
    }

    /// 处理调度实体的时间片到期事
    pub fn entity_tick(&mut self, curr: Arc<FairSchedEntity>, queued: bool) {
        // 更新当前调度实体的运行时间统计信息
        self.update_current();

        // TODO: 更新SMP相关

        if self.nr_running > 1 {}
    }

    pub fn update_current(&mut self) {
        let curr = self.current();
        let now = self.rq.clock_task;
        if unlikely(curr.is_none()) {
            return;
        }

        let curr = curr.unwrap();

        if unlikely(now <= curr.exec_start) {
            return;
        }

        let delta_exec = now - curr.exec_start;

        let curr = curr.force_mut();

        curr.sum_exec_runtime += delta_exec;

        // 根据实际运行时长加权增加虚拟运行时长
        curr.vruntime += curr.calculate_delta_fair(delta_exec);

        self.update_min_vruntime();
    }

    /// ## 更新最小虚拟运行时间
    pub fn update_min_vruntime(&mut self) {
        let curr = self.current();

        // 找到最小虚拟运行时间的调度实体
        let leftmost = self.entities.get_first();

        let mut vruntime = self.min_vruntime;

        if curr.is_some() {
            let curr = curr.as_ref().unwrap();
            if curr.on_rq {
                vruntime = curr.vruntime;
            } else {
                self.current = Weak::default();
            }
        }

        if leftmost.is_some() {
            let se = leftmost.unwrap().1;

            if curr.is_none() {
                vruntime = se.vruntime;
            } else {
                vruntime = vruntime.min(se.vruntime);
            }
        }

        self.min_vruntime = self.min_vruntime.max(vruntime);
    }
}

impl Scheduler for CompletelyFairScheduler {
    fn enqueue(&self, pcb: Arc<crate::process::ProcessControlBlock>, flags: u32) {
        todo!()
    }

    fn dequeue(&self, pcb: Arc<crate::process::ProcessControlBlock>, flags: u32) {
        todo!()
    }

    fn yield_task(&self) -> bool {
        todo!()
    }

    fn check_preempt_currnet(&self, pcb: Arc<crate::process::ProcessControlBlock>, flags: u32) {
        todo!()
    }

    fn pick_next_task(&self) -> Arc<crate::process::ProcessControlBlock> {
        todo!()
    }

    fn tick(&self, pcb: Arc<crate::process::ProcessControlBlock>, queued: bool) {
        todo!()
    }
}
