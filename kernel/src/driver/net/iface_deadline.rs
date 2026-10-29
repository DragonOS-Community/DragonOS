use super::*;

impl IfaceCommon {
    /// Atomically classify and, when due, claim this interface's deadlines.
    #[inline]
    pub fn classify_poll_deadline(&self, now_us: u64) -> DueResult {
        self.poll_deadlines.classify_and_claim(now_us)
    }

    /// Restore deadlines after a failed scheduler handoff without overwriting
    /// a concurrent publication from either source.
    #[inline]
    pub fn restore_poll_deadline(&self, claims: DeadlineClaims) -> bool {
        self.poll_deadlines.restore_claimed_if_empty(claims)
    }

    pub(super) fn defer_local_output_retry_at(&self, retry_at: smoltcp::time::Instant) {
        let now_us = crate::time::Instant::now().total_micros().max(0) as u64;
        let retry_us = (retry_at.total_micros().max(0) as u64).max(now_us.saturating_add(1));
        self.publish_local_output_retry(now_us, retry_us);
    }

    pub(super) fn publish_local_output_retry(&self, now_us: u64, retry_us: u64) {
        let rearm = self
            .poll_deadlines
            .publish_local_output_future(now_us, retry_us)
            == PublishResult::RearmRequired;
        self.notify_deadline_rearm(rearm);
    }

    /// Publish smoltcp's next scheduling decision while both smoltcp
    /// serialization locks are held.
    ///
    /// The returned boolean pair is `(poll_again, deadline_rearm)`.
    pub(super) fn publish_poll_deadline(
        &self,
        now: smoltcp::time::Instant,
        poll_at: Option<smoltcp::time::Instant>,
    ) -> (bool, bool) {
        match poll_at {
            Some(instant) if instant <= now => {
                self.poll_deadlines.disarm_protocol();
                (true, false)
            }
            Some(instant) => {
                let now_us = now.total_micros() as u64;
                let deadline_us = instant.total_micros() as u64;
                let rearm = self
                    .poll_deadlines
                    .publish_protocol_future(now_us, deadline_us)
                    == PublishResult::RearmRequired;
                (false, rearm)
            }
            None => {
                self.poll_deadlines.disarm_protocol();
                (false, false)
            }
        }
    }

    pub(super) fn notify_deadline_rearm(&self, rearm: bool) {
        if !rearm {
            return;
        }
        if let Some(netns) = self.net_namespace() {
            netns.notify_deadline_changed();
        }
    }
}
