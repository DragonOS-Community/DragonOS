//! Existing relative wall timer engine. Async callbacks use object identity.
use super::PosixIntervalTimer;
use crate::time::{
    jiffies::NSEC_PER_JIFFY,
    timer::{clock, Timer, TimerFunction},
};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use system_error::SystemError;
pub(super) struct WallBackend;
pub(super) fn now() -> u64 {
    clock().saturating_mul(NSEC_PER_JIFFY as u64)
}
pub(super) fn arm(timer: Weak<PosixIntervalTimer>, deadline: u64, generation: u64) -> Arc<Timer> {
    let ticks = deadline.saturating_add(NSEC_PER_JIFFY as u64 - 1) / NSEC_PER_JIFFY as u64;
    let event = Timer::new(Box::new(WallCallback { timer, generation }), ticks);
    event.activate();
    event
}
#[derive(Debug)]
struct WallCallback {
    timer: Weak<PosixIntervalTimer>,
    generation: u64,
}
impl TimerFunction for WallCallback {
    fn run(&mut self) -> Result<(), SystemError> {
        if let Some(timer) = self.timer.upgrade() {
            timer.expire(self.generation);
        }
        Ok(())
    }
}
