use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::time::cpu_clock::{CpuClockOperation, CpuClockTarget};
use crate::time::jiffies::NSEC_PER_JIFFY;
use crate::time::timekeeping::{
    boottime_now, monotonic_coarse, monotonic_now, monotonic_raw_now, realtime_coarse, realtime_now,
};
use crate::time::PosixTimeSpec;
use system_error::SystemError;

use super::PosixClockID;

pub(crate) fn posix_clock_now(clock_id: PosixClockID) -> Result<PosixTimeSpec, SystemError> {
    Ok(match clock_id {
        PosixClockID::Realtime | PosixClockID::RealtimeAlarm => realtime_now(),
        PosixClockID::Monotonic => monotonic_now(),
        PosixClockID::Boottime | PosixClockID::BoottimeAlarm => boottime_now(),
        PosixClockID::MonotonicRaw => monotonic_raw_now(),
        PosixClockID::RealtimeCoarse => realtime_coarse(),
        PosixClockID::MonotonicCoarse => monotonic_coarse(),

        PosixClockID::ProcessCPUTimeID
        | PosixClockID::ThreadCPUTimeID
        | PosixClockID::DynamicCpuClock(_) => PosixTimeSpec::from_ns(
            CpuClockTarget::resolve(clock_id, CpuClockOperation::Gettime)?.sample_ns()?,
        ),
    })
}

pub(crate) fn posix_clock_res(clock_id: PosixClockID) -> Result<PosixTimeSpec, SystemError> {
    Ok(match clock_id {
        PosixClockID::ProcessCPUTimeID
        | PosixClockID::ThreadCPUTimeID
        | PosixClockID::DynamicCpuClock(_) => {
            CpuClockTarget::resolve(clock_id, CpuClockOperation::Validate)?.resolution()
        }
        PosixClockID::RealtimeCoarse | PosixClockID::MonotonicCoarse => {
            PosixTimeSpec::new(0, NSEC_PER_JIFFY as i64)
        }
        PosixClockID::Realtime
        | PosixClockID::RealtimeAlarm
        | PosixClockID::Monotonic
        | PosixClockID::MonotonicRaw
        | PosixClockID::Boottime
        | PosixClockID::BoottimeAlarm => {
            // Linux reports a nominal 1ns in high-resolution mode, not the
            // callback latency or the availability of each consumer's timer.
            // In particular, an AbsoluteTimer bridge allocation failure does
            // not disable nanosleep's direct DeadlineEvent backend.
            let highres = {
                let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
                crate::time::deadline::supported()
            };
            PosixTimeSpec::new(0, if highres { 1 } else { NSEC_PER_JIFFY as i64 })
        }
    })
}
