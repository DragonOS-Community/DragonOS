use core::hint::spin_loop;

use alloc::{boxed::Box, sync::Arc};
use system_error::SystemError;

use crate::{
    arch::CurrentTimeArch,
    libs::wait_queue::{TimeoutWaker, Waiter},
};

use super::{
    timekeeping::monotonic_now,
    timer::{next_n_us_timer_jiffies, Timer},
    PosixTimeSpec, TimeArch,
};

/// @brief 休眠指定时间（单位：纳秒）
///
/// @param sleep_time 指定休眠的时间
///
/// @return Ok(()) 正常完成
///
/// @return Err(SystemError) 错误码
pub fn nanosleep(sleep_time: PosixTimeSpec) -> Result<(), SystemError> {
    if !sleep_time.is_valid_timeout() {
        return Err(SystemError::EINVAL);
    }
    // 对于小于500us的时间，使用spin/rdtsc来进行定时
    if sleep_time.tv_nsec < 500000 && sleep_time.tv_sec == 0 {
        let expired_tsc: usize = CurrentTimeArch::cal_expire_cycles(sleep_time.tv_nsec as usize);
        while CurrentTimeArch::get_cycles() < expired_tsc {
            spin_loop()
        }
        return Ok(());
    }

    let sleep_deadline = monotonic_now().saturating_add_ktime(&sleep_time);
    let total_sleep_time_us = sleep_time.to_ktime_ns().div_ceil(1000);
    let (waiter, waker) = Waiter::new_pair();
    let handler: Box<TimeoutWaker> = TimeoutWaker::new(waker);
    let expiry_jiffies = next_n_us_timer_jiffies(total_sleep_time_us);
    let timer: Arc<Timer> = Timer::new(handler, expiry_jiffies);

    timer.activate();

    match waiter.wait(true) {
        Ok(()) => {
            debug_assert!(timer.timeout());
            Ok(())
        }
        Err(SystemError::ERESTARTSYS) => {
            timer.cancel();
            if timer.timeout()
                || sleep_deadline
                    .saturating_sub_timespec(&monotonic_now())
                    .is_empty()
            {
                Ok(())
            } else {
                Err(SystemError::ERESTARTSYS)
            }
        }
        Err(err) => {
            timer.cancel();
            Err(err)
        }
    }
}
