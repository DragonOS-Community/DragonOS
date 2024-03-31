use alloc::sync::Arc;
use uefi::table::runtime::Time;

use crate::time::timer::{self, InnerTimer, Timer, TimerFunction};

struct AlarmTimer{
    timer: Arc<Timer>,
    expired_time: u64,
}

impl AlarmTimer {
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_time: u64) -> Arc<Self>{
        let result: Arc<Self> = Arc::new(AlarmTimer{
            timer: Arc::new(Timer::new(timer_func, expire_time)),
            expired_time:  expire_time,
        });
    }

    pub fn inner(&self) -> SpinLockGuard<InnerTimer> {
        return self.timer.inner();
    }

    pub fn activate(&self) {
        self.timer.activate();
    }

    pub fn timeout(&self) -> bool {
        return self.timer.timeout();
    }

    pub fn cancel(&self) -> bool {
        return self.timer.cancel();
    }

}