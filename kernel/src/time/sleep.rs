use core::{arch::x86_64::_rdtsc, ptr::null_mut};

use alloc::{boxed::Box, sync::Arc};

use crate::{
    arch::{
        asm::current::current_pcb,
        interrupt::{cli, sti},
        sched::sched,
    },
    include::bindings::bindings::{timespec, useconds_t, Cpu_tsc_freq},
    kdebug,
    syscall::SystemError,
};

use super::{
    timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
    TimeSpec,
};

/// @brief 休眠指定时间（单位：纳秒）
///
/// @param sleep_time 指定休眠的时间
///
/// @return Ok(TimeSpec) 剩余休眠时间
///
/// @return Err(SystemError) 错误码
pub fn nano_sleep(sleep_time: TimeSpec) -> Result<TimeSpec, SystemError> {
    if sleep_time.tv_nsec < 0 || sleep_time.tv_nsec >= 1000000000 {
        return Err(SystemError::EINVAL);
    }
    // 对于小于500us的时间，使用spin/rdtsc来进行定时

    if sleep_time.tv_nsec < 500000 {
        let expired_tsc: u64 =
            unsafe { _rdtsc() + (sleep_time.tv_nsec as u64 * Cpu_tsc_freq) / 1000000000 };
        while unsafe { _rdtsc() } < expired_tsc {
            kdebug!("while in nano_sleep");
        }
        return Ok(TimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        });
    }
    // 创建定时器
    let handler: Box<WakeUpHelper> = WakeUpHelper::new(current_pcb());
    let timer: Arc<Timer> = Timer::new(
        handler,
        next_n_us_timer_jiffies((sleep_time.tv_nsec / 1000) as u64),
    );

    cli();
    kdebug!("nano_sleep timer.activate()");
    timer.activate();
    unsafe {
        current_pcb().mark_sleep_interruptible();
    }
    sti();

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
pub fn us_sleep(sleep_time: TimeSpec) -> Result<TimeSpec, SystemError> {
    match nano_sleep(sleep_time) {
        Ok(value) => return Ok(value),
        Err(err) => return Err(err),
    };
}

//===== 以下为提供给C的接口 =====

/// @brief 休眠指定时间（单位：纳秒）（提供给C的接口）
///
/// @param sleep_time 指定休眠的时间
///
/// @param rm_time 剩余休眠时间（传出参数）
///
/// @return Ok(i32) 0
///
/// @return Err(SystemError) 错误码
#[no_mangle]
pub extern "C" fn rs_nanosleep(
    sleep_time: *const timespec,
    rm_time: *mut timespec,
) -> i32 {
    if sleep_time == null_mut() {
        return SystemError::EINVAL.to_posix_errno();
    }
    let slt_spec = TimeSpec {
        tv_sec: unsafe { *sleep_time }.tv_sec,
        tv_nsec: unsafe { *sleep_time }.tv_nsec,
    };

    match nano_sleep(slt_spec) {
        Ok(value) => {
            if rm_time != null_mut() {
                unsafe { *rm_time }.tv_sec = value.tv_sec;
                unsafe { *rm_time }.tv_nsec = value.tv_nsec;
            }
            kdebug!("nano_sleep_c run successfully");
            return 0;
        }
        Err(err) => {
            kdebug!("nano_sleep_c run failed");
            return err.to_posix_errno();
        }
    }
}

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
    match us_sleep(sleep_time) {
        Ok(_) => {
            kdebug!("rs_us_sleep run successfully");
            return 0;
        }
        Err(err) => {
            kdebug!("rs_us_sleep run failed");
            return err.to_posix_errno();
        }
    };
}
