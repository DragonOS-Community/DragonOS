use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

pub type ErrSeqValue = u32;

const MAX_ERRNO: ErrSeqValue = SystemError::MAXERRNO as ErrSeqValue;
const ERRSEQ_SEEN: ErrSeqValue = 1 << 12;
const ERRSEQ_CTR_INC: ErrSeqValue = 1 << 13;

#[derive(Debug, Default)]
pub struct ErrSeq {
    value: AtomicU32,
}

impl ErrSeq {
    pub const fn new() -> Self {
        Self {
            value: AtomicU32::new(0),
        }
    }

    /// Retire a detached mapping's errno after draining all error producers.
    /// Keep a distinct, sampled epoch: live file cursors can outlast a medium,
    /// and resetting to zero would let a later error reuse their encoded value.
    pub(crate) fn clear(&self) {
        let _ = self
            .value
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some((old & !(MAX_ERRNO | ERRSEQ_SEEN)).wrapping_add(ERRSEQ_CTR_INC) | ERRSEQ_SEEN)
            });
    }

    pub fn sample(&self) -> ErrSeqValue {
        let old = self.value.load(Ordering::Acquire);
        if old & ERRSEQ_SEEN == 0 {
            0
        } else {
            old
        }
    }

    pub fn set(&self, error: SystemError) -> ErrSeqValue {
        let errno = error as ErrSeqValue;
        let mut old = self.value.load(Ordering::Acquire);

        if errno == 0 || errno > MAX_ERRNO {
            return old;
        }

        loop {
            let mut new = (old & !(MAX_ERRNO | ERRSEQ_SEEN)) | errno;
            if old & ERRSEQ_SEEN != 0 {
                new = new.wrapping_add(ERRSEQ_CTR_INC);
            }

            if new == old {
                return old;
            }

            match self
                .value
                .compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return old,
                Err(cur) if cur == new => return cur,
                Err(cur) => old = cur,
            }
        }
    }

    pub fn check(&self, since: ErrSeqValue) -> Option<SystemError> {
        let cur = self.value.load(Ordering::Acquire);
        if cur == since {
            None
        } else {
            Self::error_from_value(cur)
        }
    }

    pub fn check_and_advance(&self, since: &mut ErrSeqValue) -> Option<SystemError> {
        let old = self.value.load(Ordering::Acquire);
        if old == *since {
            return None;
        }

        let new = old | ERRSEQ_SEEN;
        if new != old {
            let _ = self
                .value
                .compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire);
        }

        *since = new;
        Self::error_from_value(new)
    }

    fn error_from_value(value: ErrSeqValue) -> Option<SystemError> {
        let errno = value & MAX_ERRNO;
        if errno == 0 {
            None
        } else {
            SystemError::from_posix_errno(-(errno as i32))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_error_is_visible_to_new_sample() {
        let errseq = ErrSeq::new();
        errseq.set(SystemError::EIO);

        let mut sample = errseq.sample();
        assert_eq!(sample, 0);
        assert_eq!(
            errseq.check_and_advance(&mut sample),
            Some(SystemError::EIO)
        );
        assert_eq!(errseq.check_and_advance(&mut sample), None);
    }

    #[test]
    fn multiple_watchers_observe_same_error_once() {
        let errseq = ErrSeq::new();
        let mut first = errseq.sample();
        let mut second = errseq.sample();

        errseq.set(SystemError::ENOSPC);

        assert_eq!(
            errseq.check_and_advance(&mut first),
            Some(SystemError::ENOSPC)
        );
        assert_eq!(
            errseq.check_and_advance(&mut second),
            Some(SystemError::ENOSPC)
        );
        assert_eq!(errseq.check_and_advance(&mut first), None);
        assert_eq!(errseq.check_and_advance(&mut second), None);
    }

    #[test]
    fn detached_mapping_does_not_reuse_a_live_cursor() {
        let errseq = ErrSeq::new();
        let mut original = errseq.sample();
        errseq.set(SystemError::EIO);
        assert_eq!(
            errseq.check_and_advance(&mut original),
            Some(SystemError::EIO)
        );
        errseq.clear();
        assert_eq!(errseq.check(original), None);
        let mut second = errseq.sample();
        errseq.set(SystemError::EIO);
        assert_eq!(
            errseq.check_and_advance(&mut second),
            Some(SystemError::EIO)
        );
        assert_eq!(
            errseq.check_and_advance(&mut original),
            Some(SystemError::EIO)
        );
        assert_eq!(errseq.check_and_advance(&mut original), None);
    }

    #[test]
    fn detach_discards_unseen_error_without_reporting_it_to_a_new_sample() {
        let errseq = ErrSeq::new();
        let mut original = errseq.sample();
        errseq.set(SystemError::ENOSPC);
        errseq.clear();
        assert_eq!(errseq.check(original), None);
        let mut late = errseq.sample();
        assert_eq!(errseq.check_and_advance(&mut late), None);
        errseq.set(SystemError::EIO);
        assert_eq!(errseq.check_and_advance(&mut late), Some(SystemError::EIO));
        assert_eq!(
            errseq.check_and_advance(&mut original),
            Some(SystemError::EIO)
        );
    }

    #[test]
    fn repeated_detaches_keep_same_errno_visible_to_live_watchers() {
        let errseq = ErrSeq::new();
        let mut original = errseq.sample();
        for _ in 0..32 {
            errseq.clear();
            errseq.clear();
            let mut late = errseq.sample();
            assert_eq!(errseq.check_and_advance(&mut late), None);
            errseq.set(SystemError::EIO);
            assert_eq!(errseq.check_and_advance(&mut late), Some(SystemError::EIO));
            assert_eq!(
                errseq.check_and_advance(&mut original),
                Some(SystemError::EIO)
            );
            assert_eq!(errseq.check_and_advance(&mut original), None);
        }
    }

    #[test]
    fn seen_error_is_not_visible_to_late_sample() {
        let errseq = ErrSeq::new();
        let mut first = errseq.sample();

        errseq.set(SystemError::EIO);
        assert_eq!(errseq.check_and_advance(&mut first), Some(SystemError::EIO));

        let mut late = errseq.sample();
        assert_eq!(errseq.check_and_advance(&mut late), None);
    }
}
