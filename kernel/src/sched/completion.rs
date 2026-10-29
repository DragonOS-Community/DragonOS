#![allow(dead_code)]

use alloc::boxed::Box;
use system_error::SystemError;

use crate::{
    libs::{
        resource_wait::{self, ResourceWaitOutcome, ResourceWaitQueue, WaitMode},
        spinlock::SpinLock,
    },
    process::ProcessManager,
    time::timer::{clock, Timer, TimerFunction},
};

const COMPLETE_ALL: u32 = u32::MAX;
const MAX_TIMEOUT: i64 = i64::MAX;

/// A counted completion. A resource borrow pins its allocation-free wait key.
#[derive(Debug)]
pub struct Completion {
    inner: SpinLock<InnerCompletion>,
    wait_queue: ResourceWaitQueue,
}

/// The callback never dereferences the completion: cancellation can race a
/// running timer, whose late notification must remain safe after wait returns.
#[derive(Debug)]
struct CompletionTimeout(usize);
impl TimerFunction for CompletionTimeout {
    fn run(&mut self) -> Result<(), SystemError> {
        resource_wait::notify_resource_all(self.0);
        Ok(())
    }
}

impl Completion {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerCompletion::new()),
            wait_queue: ResourceWaitQueue::new(),
        }
    }

    fn consume(&self) -> bool {
        self.inner.lock_irqsave().consume()
    }

    fn completed_result(deadline: Option<u64>) -> i64 {
        deadline.map_or(MAX_TIMEOUT, |end| {
            end.saturating_sub(clock()).min(i64::MAX as u64).max(1) as i64
        })
    }

    /// Completion wins over a simultaneous timeout, as in Linux's swait loop.
    fn outcome(&self, deadline: Option<u64>) -> Option<ResourceWaitOutcome<i64>> {
        let mut inner = self.inner.lock_irqsave();
        if inner.consume() {
            Some(ResourceWaitOutcome::Acquired(Self::completed_result(
                deadline,
            )))
        } else if deadline.is_some_and(|end| clock() >= end) {
            Some(ResourceWaitOutcome::Cancelled(0))
        } else {
            None
        }
    }

    fn do_wait_for_common(&self, timeout: i64, mode: WaitMode) -> Result<i64, SystemError> {
        self.finish_wait(self.wait_inner(timeout, mode))
    }

    fn finish_wait(&self, result: Result<i64, SystemError>) -> Result<i64, SystemError> {
        // A counter can receive several releases before its first notified
        // waiter runs. Relay a remaining ticket only after wait_inner retired
        // that waiter's Registration and local count, so it cannot select
        // itself again. COMPLETE_ALL already broadcast to every waiter.
        if result.as_ref().is_ok_and(|left| *left > 0) {
            let remaining = {
                let inner = self.inner.lock_irqsave();
                inner.done != 0 && inner.done != COMPLETE_ALL
            };
            if remaining {
                self.wait_queue.notify_one();
            }
        }
        result
    }

    fn wait_inner(&self, timeout: i64, mode: WaitMode) -> Result<i64, SystemError> {
        let deadline =
            (timeout != MAX_TIMEOUT).then(|| clock().saturating_add(timeout.max(0) as u64));
        if self.consume() {
            return Ok(Self::completed_result(deadline));
        }
        // Match resource_wait admission before preparing a timed wait. A
        // pending signal must not be hidden by timer ENOMEM or timeout zero.
        let task = ProcessManager::current_pcb();
        if mode.interrupted(&task) {
            return if self.consume() {
                Ok(Self::completed_result(deadline))
            } else {
                Err(SystemError::ERESTARTSYS)
            };
        }

        // All timer storage is born before registration/prepare_sleep. An
        // untimed wait, including kthread join, never needs heap storage.
        let timer = if let Some(expiry) = deadline.filter(|end| *end > clock()) {
            let prepared = Box::try_new(CompletionTimeout(self.wait_queue.key()))
                .map_err(|_| SystemError::ENOMEM)
                .and_then(|callback| Timer::try_prepare(callback, expiry));
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    // A completion that arrived during preparation owns the
                    // result, even if timer allocation concurrently failed.
                    return if self.consume() {
                        Ok(Self::completed_result(deadline))
                    } else {
                        Err(error)
                    };
                }
            };
            let timer = prepared.timer();
            prepared.activate();
            Some(timer)
        } else {
            None
        };

        let result = self
            .wait_queue
            .wait_with_outcome(mode, || self.outcome(deadline));
        // Timer retirement/callback destruction never holds the done lock.
        if let Some(timer) = timer {
            timer.cancel();
        }
        match result {
            // Resource waits check signals before their predicate. Restore
            // completion's done-first contract on the cancellation path.
            Err(error) => {
                if self.consume() {
                    Ok(Self::completed_result(deadline))
                } else {
                    Err(error)
                }
            }
            success => success,
        }
    }

    /// Returns zero on expiry, otherwise remaining jiffies (at least one).
    pub fn wait_for_completion_timeout(&self, timeout: i64) -> Result<i64, SystemError> {
        self.do_wait_for_common(timeout, WaitMode::Uninterruptible)
    }

    pub fn wait_for_completion(&self) -> Result<i64, SystemError> {
        self.do_wait_for_common(MAX_TIMEOUT, WaitMode::Uninterruptible)
    }

    pub fn wait_for_completion_interruptible(&self) -> Result<i64, SystemError> {
        self.do_wait_for_common(MAX_TIMEOUT, WaitMode::Interruptible)
    }

    pub fn wait_for_completion_killable(&self) -> Result<i64, SystemError> {
        self.do_wait_for_common(MAX_TIMEOUT, WaitMode::Killable)
    }

    pub fn wait_for_completion_interruptible_timeout(
        &mut self,
        timeout: i64,
    ) -> Result<i64, SystemError> {
        assert!(timeout >= 0);
        self.do_wait_for_common(timeout, WaitMode::Interruptible)
    }

    pub fn complete(&self) {
        let all = {
            let mut inner = self.inner.lock_irqsave();
            if inner.done != COMPLETE_ALL {
                inner.done = inner.done.saturating_add(1);
            }
            inner.done == COMPLETE_ALL
        };
        // Saturation publishes the same permanent state as complete_all.
        if all {
            self.wait_queue.notify_all();
        } else {
            self.wait_queue.notify_one();
        }
    }

    pub fn complete_all(&self) {
        self.inner.lock_irqsave().done = COMPLETE_ALL;
        self.wait_queue.notify_all();
    }

    pub fn try_wait_for_completion(&mut self) -> bool {
        self.consume()
    }

    pub fn completion_done(&self) -> bool {
        self.inner.lock_irqsave().done != 0
    }
}

#[derive(Debug)]
pub struct InnerCompletion {
    done: u32,
}

impl InnerCompletion {
    fn consume(&mut self) -> bool {
        if self.done == 0 {
            return false;
        }
        if self.done != COMPLETE_ALL {
            self.done -= 1;
        }
        true
    }

    pub const fn new() -> Self {
        Self { done: 0 }
    }
}

#[cfg(feature = "mm_selftest")]
mod selftest;
#[cfg(feature = "mm_selftest")]
pub(crate) use selftest::wait_protocol_selftest;
