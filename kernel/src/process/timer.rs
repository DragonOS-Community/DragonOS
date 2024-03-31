use alloc::{
    boxed::Box,
    sync::Arc,
};
use crate::time::timer::{InnerTimer, Timer, TimerFunction};
use crate::libs::spinlock::SpinLockGuard;

struct AlarmTimer{
    timer: Arc<Timer>,
    expired_time: u64,
}

impl AlarmTimer {
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_time: u64) -> Arc<Self>{
        let result: Arc<Self> = Arc::new(AlarmTimer{
            timer: Timer::new(timer_func, expire_time),
            expired_time:  expire_time,
        });
        result
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