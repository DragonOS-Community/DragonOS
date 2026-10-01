//! Process-directed pending signal queue operations.
use super::*;

impl ProcessSignalState {
    /// Hold the process pending snapshot stable across ptrace state checks.
    pub(crate) fn lock_irqsave(&self) -> SpinLockGuard<'_, SigPending> {
        self.pending.lock_irqsave()
    }

    pub fn shared_pending_signal(&self) -> SigSet {
        self.pending.lock_irqsave().signal()
    }

    pub fn shared_pending_flush_by_mask(&self, mask: &SigSet) {
        self.pending.lock_irqsave().flush_by_mask(mask);
    }

    pub fn shared_pending_queue_has(&self, sig: Signal) -> bool {
        self.pending.lock_irqsave().queue().find(sig).0.is_some()
    }

    pub fn shared_pending_posix_timer_bump_overrun(
        &self,
        sig: Signal,
        timerid: i32,
        bump: i32,
    ) -> bool {
        for info in self.pending.lock_irqsave().queue_mut().q.iter_mut() {
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.bump_posix_timer_overrun(timerid, bump)
            {
                return true;
            }
        }
        false
    }

    pub fn shared_pending_posix_timer_reset_overrun(&self, sig: Signal, timerid: i32) -> bool {
        for info in self.pending.lock_irqsave().queue_mut().q.iter_mut() {
            if info.is_signal(sig)
                && info.sig_code() == SigCode::Timer
                && info.reset_posix_timer_overrun(timerid)
            {
                return true;
            }
        }
        false
    }

    pub fn shared_pending_dequeue(&self, mask: &SigSet) -> (Signal, Option<SigInfo>) {
        self.pending.lock_irqsave().dequeue_signal(mask)
    }

    pub fn shared_pending_push(&self, sig: Signal, info: SigInfo) {
        let mut pending = self.pending.lock_irqsave();
        pending.queue_mut().q.push(info);
        pending.signal_mut().insert(sig.into());
    }

    pub fn shared_pending_push_dedup(&self, sig: Signal, info: SigInfo) -> bool {
        let mut pending = self.pending.lock_irqsave();
        if !sig.is_rt_signal() && pending.queue().find(sig).0.is_some() {
            return false;
        }
        pending.queue_mut().q.push(info);
        pending.signal_mut().insert(sig.into());
        true
    }

    pub fn shared_pending_push_posix_timer(&self, sig: Signal, info: SigInfo) -> bool {
        let mut pending = self.pending.lock_irqsave();
        if pending.queue().find(sig).0.is_some() {
            return false;
        }
        pending.queue_mut().q.push(info);
        pending.signal_mut().insert(sig.into());
        true
    }

    pub fn shared_pending_signal_insert(&self, sig: Signal) {
        self.pending.lock_irqsave().signal_mut().insert(sig.into());
    }
}
