use core::hint::spin_loop;

use alloc::{boxed::Box, sync::Arc};
use system_error::SystemError;

use crate::{
    arch::{CurrentIrqArch, CurrentTimeArch},
    exception::InterruptArch,
    process::ProcessManager,
    sched::{schedule, SchedMode},
    time::timekeeping::getnstimeofday,
};

use super::{
    timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
    PosixTimeSpec, TimeArch,
};

/// @brief 休眠指定时间（单位：纳秒）
///
/// @param sleep_time 指定休眠的时间
///
/// @return Ok(TimeSpec) 剩余休眠时间
///
/// @return Err(SystemError) 错误码
pub fn nanosleep(sleep_time: PosixTimeSpec) -> Result<PosixTimeSpec, SystemError> {
    if sleep_time.tv_nsec < 0 || sleep_time.tv_nsec >= 1000000000 {
        return Err(SystemError::EINVAL);
    }
    // 对于小于500us的时间，使用spin/rdtsc来进行定时
    if sleep_time.tv_nsec < 500000 && sleep_time.tv_sec == 0 {
        let expired_tsc: usize = CurrentTimeArch::cal_expire_cycles(sleep_time.tv_nsec as usize);
        while CurrentTimeArch::get_cycles() < expired_tsc {
            spin_loop()
        }
        return Ok(PosixTimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        });
    }

    let total_sleep_time_us: u64 =
        sleep_time.tv_sec as u64 * 1000000 + sleep_time.tv_nsec as u64 / 1000;
    // 创建定时器
    let handler: Box<WakeUpHelper> = WakeUpHelper::new(ProcessManager::current_pcb());
    let timer: Arc<Timer> = Timer::new(handler, next_n_us_timer_jiffies(total_sleep_time_us));

    let start_time = getnstimeofday();
    timer.activate();

    // Linux 语义：等待可能出现伪唤醒；只有在收到未被屏蔽的信号时才中断。
    // 对于 job-control stop/continue 这类不应直接对用户态暴露的唤醒，应继续等待直到到期。
    loop {
        let irq_guard: crate::exception::IrqFlagsGuard =
            unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).ok();
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);

        if timer.timeout() {
            break;
        }

        // 未到期而被唤醒：若存在未屏蔽待处理信号，则视为被信号打断；否则认为是伪唤醒，继续等待。
        let current_pcb = ProcessManager::current_pcb();
        let has_real_signal =
            current_pcb.has_pending_signal_fast() && current_pcb.has_pending_not_masked_signal();
        if has_real_signal {
            timer.cancel();
            return Err(SystemError::ERESTARTSYS);
        }
    }

    let end_time = getnstimeofday();
    let real_sleep_time = end_time - start_time;
    let rm_time: PosixTimeSpec = (sleep_time - real_sleep_time.into()).into();
    Ok(rm_time)
}
