use crate::process::ProcessManager;
use crate::time::jiffies::NSEC_PER_JIFFY;
use crate::time::timekeeping::{
    boottime_now, monotonic_coarse, monotonic_now, monotonic_raw_now, realtime_coarse, realtime_now,
};
use crate::time::PosixTimeSpec;

use super::{PosixClockID, CPUCLOCK_PERTHREAD_MASK};

pub(crate) fn posix_clock_now(clock_id: PosixClockID) -> PosixTimeSpec {
    match clock_id {
        PosixClockID::Realtime | PosixClockID::RealtimeAlarm => realtime_now(),
        PosixClockID::Monotonic => monotonic_now(),
        PosixClockID::Boottime | PosixClockID::BoottimeAlarm => boottime_now(),
        PosixClockID::MonotonicRaw => monotonic_raw_now(),
        PosixClockID::RealtimeCoarse => realtime_coarse(),
        PosixClockID::MonotonicCoarse => monotonic_coarse(),

        PosixClockID::ProcessCPUTimeID => {
            let pcb = ProcessManager::current_pcb();
            PosixTimeSpec::from_ns(pcb.process_cputime_ns())
        }
        PosixClockID::ThreadCPUTimeID => {
            let pcb = ProcessManager::current_pcb();
            PosixTimeSpec::from_ns(pcb.thread_cputime_ns())
        }
        // Dynamic CPU clock ID from pthread_getcpuclockid()
        // Extract the per-thread flag to determine whether to return thread or process CPU time
        PosixClockID::DynamicCpuClock(raw) => {
            let pcb = ProcessManager::current_pcb();
            // Bit 2 (CPUCLOCK_PERTHREAD_MASK) indicates whether this is a per-thread clock
            let is_per_thread = (raw & CPUCLOCK_PERTHREAD_MASK) != 0;
            if is_per_thread {
                PosixTimeSpec::from_ns(pcb.thread_cputime_ns())
            } else {
                PosixTimeSpec::from_ns(pcb.process_cputime_ns())
            }
        }
    }
}

pub(crate) fn posix_clock_res(clock_id: PosixClockID) -> PosixTimeSpec {
    match clock_id {
        PosixClockID::ProcessCPUTimeID
        | PosixClockID::ThreadCPUTimeID
        | PosixClockID::DynamicCpuClock(_) => PosixTimeSpec::new(0, 1),
        _ => PosixTimeSpec::new(0, NSEC_PER_JIFFY as i64),
    }
}
