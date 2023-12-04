use core::hint::spin_loop;

use alloc::{boxed::Box, sync::Arc};

use crate::{
    arch::{sched::sched, CurrentIrqArch, CurrentTimeArch},
    exception::InterruptArch,
    include::bindings::bindings::{useconds_t, Cpu_tsc_freq},
    process::ProcessManager,
    syscall::SystemError,
};

use super::{
    timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
    TimeArch, TimeSpec,
};

/// @brief 休眠指定时间（单位：纳秒）
///
/// @param sleep_time 指定休眠的时间
///
/// @return Ok(TimeSpec) 剩余休眠时间
///
/// @return Err(SystemError) 错误码
pub fn nanosleep(sleep_time: TimeSpec) -> Result<TimeSpec, SystemError> {
    if sleep_time.tv_nsec < 0 || sleep_time.tv_nsec >= 1000000000 {
        return Err(SystemError::EINVAL);
    }
    // 对于小于500us的时间，使用spin/rdtsc来进行定时

    if sleep_time.tv_nsec < 500000 {
        let expired_tsc: u64 = unsafe {
            CurrentTimeArch::get_cycles() as u64
                + (sleep_time.tv_nsec as u64 * Cpu_tsc_freq) / 1000000000
        };
        while (CurrentTimeArch::get_cycles() as u64) < expired_tsc {
            spin_loop()
        }
        return Ok(TimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        });
    }
    // 创建定时器
    let handler: Box<WakeUpHelper> = WakeUpHelper::new(ProcessManager::current_pcb());
    let timer: Arc<Timer> = Timer::new(
        handler,
        next_n_us_timer_jiffies((sleep_time.tv_nsec / 1000) as u64),
    );

    let irq_guard: crate::exception::IrqFlagsGuard =
        unsafe { CurrentIrqArch::save_and_disable_irq() };
    ProcessManager::mark_sleep(true).ok();
    timer.activate();

    drop(irq_guard);

    sched();

    // TODO: 增加信号唤醒的功能后，返回正确的剩余时间

    return Ok(TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    });
}

/// @brief 休眠指定时间（单位：微秒）
///
///  @param usec 微秒
///
/// @return Ok(TimeSpec) 剩余休眠时间
///
/// @return Err(SystemError) 错误码
pub fn usleep(sleep_time: TimeSpec) -> Result<TimeSpec, SystemError> {
    match nanosleep(sleep_time) {
        Ok(value) => return Ok(value),
        Err(err) => return Err(err),
    };
}

//===== 以下为提供给C的接口 =====

/// @brief 休眠指定时间（单位：微秒）（提供给C的接口）
///
///  @param usec 微秒
///
/// @return Ok(i32) 0
///
/// @return Err(SystemError) 错误码
#[no_mangle]
pub extern "C" fn rs_usleep(usec: useconds_t) -> i32 {
    let sleep_time = TimeSpec {
        tv_sec: (usec / 1000000) as i64,
        tv_nsec: ((usec % 1000000) * 1000) as i64,
    };
    match usleep(sleep_time) {
        Ok(_) => {
            return 0;
        }
        Err(err) => {
            return err.to_posix_errno();
        }
    };
}
