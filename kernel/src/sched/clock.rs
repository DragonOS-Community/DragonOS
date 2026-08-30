//! 这个文件实现的是调度过程中涉及到的时钟
//!
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::{arch::CurrentTimeArch, smp::cpu::ProcessorId, time::TimeArch};

#[cfg(target_arch = "x86_64")]
const HALF_RANGE: u64 = 1_u64 << 63;
#[cfg(target_arch = "x86_64")]
const CLOCK_UNINITIALIZED: u8 = 0;
#[cfg(target_arch = "x86_64")]
const CLOCK_INITIALIZING: u8 = 1;
#[cfg(target_arch = "x86_64")]
const CLOCK_INITIALIZED: u8 = 2;

#[cfg(target_arch = "x86_64")]
#[repr(align(64))]
struct SchedClockData {
    tick_raw: AtomicU64,
    tick_clock: AtomicU64,
    clock: AtomicU64,
    state: AtomicU8,
}

#[cfg(target_arch = "x86_64")]
impl SchedClockData {
    const fn new() -> Self {
        Self {
            tick_raw: AtomicU64::new(0),
            tick_clock: AtomicU64::new(0),
            clock: AtomicU64::new(0),
            state: AtomicU8::new(CLOCK_UNINITIALIZED),
        }
    }

    fn initialize(&self, raw: u64, global: u64) -> bool {
        match self.state.compare_exchange(
            CLOCK_UNINITIALIZED,
            CLOCK_INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(CLOCK_INITIALIZED) => return true,
            Err(_) => return false,
        }
        let old = self.clock.load(Ordering::Acquire);
        let anchor = advance_clock(self, later_clock(global, old));
        self.tick_raw.store(raw, Ordering::Relaxed);
        self.tick_clock.store(anchor, Ordering::Relaxed);
        self.state.store(CLOCK_INITIALIZED, Ordering::Release);
        true
    }
}

#[cfg(target_arch = "x86_64")]
static SCHED_CLOCK_DATA: [SchedClockData; crate::mm::percpu::PerCpu::MAX_CPU_NUM as usize] =
    [const { SchedClockData::new() }; crate::mm::percpu::PerCpu::MAX_CPU_NUM as usize];

#[cfg(target_arch = "x86_64")]
#[inline]
const fn clock_after(candidate: u64, reference: u64) -> bool {
    candidate != reference && candidate.wrapping_sub(reference) < HALF_RANGE
}

#[cfg(target_arch = "x86_64")]
#[inline]
const fn later_clock(candidate: u64, reference: u64) -> u64 {
    if clock_after(candidate, reference) {
        candidate
    } else {
        reference
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
const fn corrected_clock_value(raw: u64, tick_raw: u64, tick_clock: u64, old: u64) -> u64 {
    let raw_delta = raw.wrapping_sub(tick_raw);
    let delta = if raw_delta < HALF_RANGE { raw_delta } else { 0 };
    let candidate = tick_clock.wrapping_add(delta);
    if clock_after(candidate, old) {
        candidate
    } else {
        old
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn raw_sched_clock() -> u64 {
    if crate::arch::driver::tsc::TSCManager::cpu_khz() == 0 {
        0
    } else {
        CurrentTimeArch::cycles2ns(CurrentTimeArch::get_cycles()) as u64
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn global_tick_clock() -> u64 {
    crate::time::timer::clock().wrapping_mul(crate::time::jiffies::NSEC_PER_JIFFY as u64)
}

pub struct SchedClock;

impl SchedClock {
    /// Couples the current CPU's raw clock to the global tick epoch.
    ///
    /// If CPU0 later stops ticking, the last raw anchor remains in place and
    /// healthy CPUs continue advancing from their invariant local counters.
    #[inline]
    pub fn tick(cpu: ProcessorId) {
        #[cfg(target_arch = "x86_64")]
        {
            let raw = raw_sched_clock();
            if raw == 0 {
                return;
            }
            let global = global_tick_clock();
            let data = &SCHED_CLOCK_DATA[cpu.data() as usize];
            if !data.initialize(raw, global) {
                return;
            }
            let local = update_local_clock(data, raw);
            let anchor = if clock_after(global, local) {
                advance_clock(data, global)
            } else {
                local
            };
            // Never re-anchor to a lagging global tick. This preserves the
            // progress accumulated from the local counter while CPU0 was not
            // ticking.
            data.tick_raw.store(raw, Ordering::Relaxed);
            data.tick_clock.store(anchor, Ordering::Release);
        }

        #[cfg(not(target_arch = "x86_64"))]
        let _ = cpu;
    }

    pub fn reset_cpu(cpu: ProcessorId) {
        #[cfg(target_arch = "x86_64")]
        SCHED_CLOCK_DATA[cpu.data() as usize]
            .state
            .store(CLOCK_UNINITIALIZED, Ordering::Release);

        #[cfg(not(target_arch = "x86_64"))]
        let _ = cpu;
    }

    #[inline]
    pub fn sched_clock_cpu(cpu: ProcessorId) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            let raw = raw_sched_clock();
            if raw == 0 {
                return 0;
            }
            let current_cpu = crate::smp::core::smp_get_processor_id();
            let local_data = &SCHED_CLOCK_DATA[current_cpu.data() as usize];
            if !local_data.initialize(raw, global_tick_clock()) {
                return local_data.clock.load(Ordering::Acquire);
            }
            let local = update_local_clock(local_data, raw);
            if cpu == current_cpu {
                return local;
            }

            // A remote CPU's raw counter cannot be sampled here: it may have
            // a different TSC offset. Couple its already-corrected clock to
            // this CPU's comparable local value instead.
            let remote = &SCHED_CLOCK_DATA[cpu.data() as usize];
            if remote.state.load(Ordering::Acquire) != CLOCK_INITIALIZED {
                return local;
            }
            return advance_clock(remote, local);
        }

        #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
        {
            return CurrentTimeArch::cycles2ns(CurrentTimeArch::get_cycles()) as u64;
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn advance_clock(data: &SchedClockData, candidate: u64) -> u64 {
    let mut old = data.clock.load(Ordering::Acquire);
    while clock_after(candidate, old) {
        match data
            .clock
            .compare_exchange_weak(old, candidate, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return candidate,
            Err(actual) => old = actual,
        }
    }
    old
}

#[cfg(target_arch = "x86_64")]
fn update_local_clock(data: &SchedClockData, raw: u64) -> u64 {
    let tick_clock = data.tick_clock.load(Ordering::Acquire);
    let tick_raw = data.tick_raw.load(Ordering::Relaxed);
    let old = data.clock.load(Ordering::Acquire);
    advance_clock(data, corrected_clock_value(raw, tick_raw, tick_clock, old))
}

pub(crate) fn run_sched_clock_selftests() -> Result<(), &'static str> {
    #[cfg(target_arch = "x86_64")]
    {
        let global = 5_000_000;
        let cpu0 = corrected_clock_value(1_020_000, 1_000_000, global, global);
        let cpu1 = corrected_clock_value(9_020_000, 9_000_000, global, global);
        if cpu0 != global + 20_000 || cpu1 != cpu0 {
            return Err("sched clock did not normalize per-CPU counter offsets");
        }
        if corrected_clock_value(2_020_000, 1_000_000, global, cpu0) != global + 1_020_000 {
            return Err("sched clock stopped without a newer global tick anchor");
        }
        let advanced = global + 1_020_000;
        if later_clock(global + 10_000, advanced) != advanced {
            return Err("sched clock reset discarded monotonic progress");
        }
        let resumed_anchor = later_clock(global + 10_000, advanced);
        if corrected_clock_value(2_030_000, 2_020_000, resumed_anchor, advanced)
            != advanced + 10_000
        {
            return Err("sched clock froze after a lagging global tick resumed");
        }
        if corrected_clock_value(999_000, 1_000_000, global, cpu0) != cpu0 {
            return Err("sched clock accepted backward local-counter motion");
        }
        let wrapped = corrected_clock_value(5, u64::MAX - 10, u64::MAX - 20, u64::MAX - 20);
        if wrapped != u64::MAX - 4 {
            return Err("sched clock lost a valid wrapping delta");
        }
    }
    Ok(())
}

bitflags! {
    pub struct ClockUpdataFlag: u8 {
        /// 请求在下一次调用 __schedule() 时跳过时钟更新
        const RQCF_REQ_SKIP = 0x01;
        /// 表示跳过时钟更新正在生效，update_rq_clock() 的调用将被忽略。
        const RQCF_ACT_SKIP = 0x02;
        /// 调试标志，指示自上次固定 rq::lock 以来是否已调用过
        const RQCF_UPDATE = 0x04;
    }
}
