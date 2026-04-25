use crate::process::ProcessManager;
use crate::time::timekeeping::getnstimeofday;
use crate::time::PosixTimeSpec;

use super::{PosixClockID, CPUCLOCK_PERTHREAD_MASK};

pub(crate) fn posix_clock_now(clock_id: PosixClockID) -> PosixTimeSpec {
    match clock_id {
        PosixClockID::Realtime => getnstimeofday(),
        // 单调/boottime/raw/coarse/alarm 等目前仍复用 realtime（后续可补齐真正语义）。
        PosixClockID::Monotonic
        | PosixClockID::Boottime
        | PosixClockID::MonotonicRaw
        | PosixClockID::RealtimeCoarse
        | PosixClockID::MonotonicCoarse
        | PosixClockID::RealtimeAlarm
        | PosixClockID::BoottimeAlarm => getnstimeofday(),

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

pub(crate) fn posix_clock_res(_clock_id: PosixClockID) -> PosixTimeSpec {
    // Linux 的 clock_getres 对大多数 POSIX clock 返回“该 clock 的分辨率”。
    // DragonOS 目前对多数 clock 仍复用 realtime/getnstimeofday() 语义，
    // 且内部时间统一用纳秒表示，因此先返回 1ns 作为分辨率。
    //
    // 后续若区分 coarse/raw 或引入 tick 粒度限制，可在这里统一收敛实现。
    PosixTimeSpec::new(0, 1)
}
