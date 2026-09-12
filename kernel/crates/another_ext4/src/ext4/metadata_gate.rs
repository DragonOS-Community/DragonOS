//! Metadata view exclusion and cancellation-safe writer preference.
use crate::prelude::*;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Non-blocking gate separating legacy direct writers from journal snapshots.
///
/// The top bit denotes an exclusive transactional owner; the remaining bits
/// count direct writers. Acquisition is lock-free rather than per-caller
/// wait-free: it never sleeps or waits for an incompatible owner, which is
/// essential because guards intentionally span block-device I/O.
pub trait MetadataMutationWaker: Send + Sync {
    /// Wake every upper-layer waiter which may have observed the previous
    /// metadata-mutation generation.
    ///
    /// This callback runs after a guard releases the atomic gate state or when
    /// the filesystem first enters fail-stop. It must not block or call back
    /// into this filesystem.
    fn wake_all(&self);
}

pub(super) struct MetadataMutationGate {
    pub(super) state: AtomicUsize,
    pub(super) generation: AtomicU64,
    waker_installed: AtomicBool,
    waker: spin::Once<Arc<dyn MetadataMutationWaker>>,
    waiting_writers: AtomicUsize,
}

const METADATA_GATE_EXCLUSIVE: usize = 1usize << (usize::BITS - 1);
pub(super) const METADATA_GATE_DIRECT_MAX: usize = METADATA_GATE_EXCLUSIVE - 1;

impl MetadataMutationGate {
    pub(super) const fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            waker_installed: AtomicBool::new(false),
            waker: spin::Once::new(),
            waiting_writers: AtomicUsize::new(0),
        }
    }

    pub(super) fn install_waker(&self, waker: Arc<dyn MetadataMutationWaker>) -> Result<()> {
        if self
            .waker_installed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.waker.call_once(|| waker);
        Ok(())
    }

    #[inline]
    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(super) fn notify_progress(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        if let Some(waker) = self.waker.get() {
            waker.wake_all();
        }
    }

    pub(super) fn try_direct(&self) -> Result<MetadataMutationGuard<'_>> {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & METADATA_GATE_EXCLUSIVE != 0 {
                return Err(Ext4Error::new(ErrCode::EAGAIN));
            }
            if state == METADATA_GATE_DIRECT_MAX {
                // This is a corrupted/impossible owner count, not contention:
                // no finite gate-release event can make a fabricated maximum
                // count a safe acquisition.
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(MetadataMutationGuard {
                        gate: self,
                        exclusive: false,
                    });
                }
                // Retry only a compatible direct-count collision. Observing an
                // exclusive owner is rejected at the top of the next iteration;
                // no acquisition waits for an I/O-spanning owner to depart.
                //
                // Do not turn a retry limit into EAGAIN: generation advances
                // only when the last direct owner exits, so compatible count
                // churn has no matching progress event for an upper-layer
                // waiter. Such a rejection could strand an otherwise
                // compatible caller until the whole direct cohort drains.
                Err(observed) => state = observed,
            }
        }
    }

    pub(super) fn try_transactional(&self) -> Result<MetadataMutationGuard<'_>> {
        self.state
            .compare_exchange(
                0,
                METADATA_GATE_EXCLUSIVE,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| Ext4Error::new(ErrCode::EAGAIN))?;
        Ok(MetadataMutationGuard {
            gate: self,
            exclusive: true,
        })
    }

    /// Register only an actual gate wait, never a journal-capacity wait at an
    /// idle gate. The retry owner keeps this capability through its next
    /// operation attempt and drops it on success, cancellation or any error.
    pub(super) fn writer_wait(&self) -> Result<Option<MetadataWriterWait<'_>>> {
        if self.state.load(Ordering::Acquire) == 0 {
            return Ok(None);
        }
        self.waiting_writers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| Ext4Error::new(ErrCode::EIO))?;
        let intent = MetadataWriterWait { gate: self };
        // If the cohort drained during registration, the retry can use its
        // generation wakeup without leaving admission closed while idle.
        if self.state.load(Ordering::Acquire) == 0 {
            drop(intent);
            return Ok(None);
        }
        Ok(Some(intent))
    }

    /// Public read views may not extend a cohort in front of an already
    /// waiting writer. Direct mutation guards retain their existing class:
    /// these short reservation/data operations may be the wait owner's retry
    /// and must not reject themselves because their intent is still alive.
    pub(super) fn try_read_view(&self) -> Result<MetadataMutationGuard<'_>> {
        if self.waiting_writers.load(Ordering::Acquire) != 0 {
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }
        let guard = self.try_direct()?;
        if self.waiting_writers.load(Ordering::Acquire) != 0 {
            drop(guard);
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }
        Ok(guard)
    }
}

/// A scoped preference for an operation retry, not ownership of the gate.
/// There is no persistent pending bit: abandoning the retry always reopens
/// reader admission once the final writer waiter has gone away.
#[must_use = "keep the preference alive through the wait and next operation attempt"]
pub struct MetadataWriterWait<'a> {
    gate: &'a MetadataMutationGate,
}

impl Drop for MetadataWriterWait<'_> {
    fn drop(&mut self) {
        let previous = self.gate.waiting_writers.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0);
        if previous == 1 {
            self.gate.notify_progress();
        }
    }
}

pub(crate) struct MetadataMutationGuard<'a> {
    gate: &'a MetadataMutationGate,
    exclusive: bool,
}

impl core::fmt::Debug for MetadataMutationGuard<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MetadataMutationGuard")
            .field("exclusive", &self.exclusive)
            .finish_non_exhaustive()
    }
}

impl Drop for MetadataMutationGuard<'_> {
    fn drop(&mut self) {
        if self.exclusive {
            debug_assert_eq!(
                self.gate.state.load(Ordering::Relaxed),
                METADATA_GATE_EXCLUSIVE
            );
            self.gate.state.store(0, Ordering::Release);
            self.gate.notify_progress();
        } else {
            let previous = self.gate.state.fetch_sub(1, Ordering::Release);
            debug_assert!(previous > 0 && previous < METADATA_GATE_EXCLUSIVE);
            if previous == 1 {
                self.gate.notify_progress();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_writer_stops_a_continuously_overlapping_read_cohort() {
        let gate = MetadataMutationGate::new();
        let first = gate.try_read_view().unwrap();
        let second = gate.try_read_view().unwrap();
        assert_eq!(
            gate.try_transactional().unwrap_err().code(),
            ErrCode::EAGAIN
        );
        let observed = gate.generation();
        let preference = gate.writer_wait().unwrap().unwrap();
        drop(first);
        // Without writer preference a replacement first reader could enter
        // here and keep the cohort alive forever across second's release.
        assert_eq!(gate.try_read_view().unwrap_err().code(), ErrCode::EAGAIN);
        assert_eq!(gate.generation(), observed);
        drop(second);
        assert_ne!(gate.generation(), observed);
        let writer = gate.try_transactional().unwrap();
        assert_eq!(gate.try_read_view().unwrap_err().code(), ErrCode::EAGAIN);
        drop(writer);
        drop(preference);
        assert!(gate.try_read_view().is_ok());
    }

    #[test]
    fn cancelled_writer_reopens_read_admission_while_other_readers_continue() {
        let gate = MetadataMutationGate::new();
        let reader = gate.try_read_view().unwrap();
        let first = gate.writer_wait().unwrap().unwrap();
        let second = gate.writer_wait().unwrap().unwrap();
        let observed = gate.generation();
        drop(first);
        assert_eq!(gate.generation(), observed);
        assert!(gate.try_read_view().is_err());
        drop(second);
        assert_ne!(gate.generation(), observed);
        assert!(gate.try_read_view().is_ok());
        drop(reader);
        assert!(gate.try_transactional().is_ok());
    }

    #[test]
    fn capacity_wait_does_not_close_read_admission_at_an_idle_gate() {
        let gate = MetadataMutationGate::new();
        assert!(gate.writer_wait().unwrap().is_none());
        assert_eq!(gate.generation(), 0);
        assert!(gate.try_read_view().is_ok());
    }

    #[test]
    fn compatible_mutation_retry_does_not_reject_its_own_preference() {
        let gate = MetadataMutationGate::new();
        let writer = gate.try_transactional().unwrap();
        let preference = gate.writer_wait().unwrap().unwrap();
        drop(writer);
        let direct = gate.try_direct().unwrap();
        drop(preference);
        drop(direct);
        assert!(gate.try_read_view().is_ok());
    }

    #[test]
    fn retry_rearms_after_its_own_cancellation_notification() {
        let gate = MetadataMutationGate::new();
        let reader = gate.try_read_view().unwrap();
        let previous = gate.writer_wait().unwrap().unwrap();
        assert!(gate.try_transactional().is_err());
        drop(previous);
        let after_release = gate.generation();
        let next = gate.writer_wait().unwrap().unwrap();
        // Registration itself is not progress. A scheduler waiter using this
        // token sleeps until the live owner releases, rather than self-waking.
        assert_eq!(gate.generation(), after_release);
        drop(reader);
        assert_ne!(gate.generation(), after_release);
        drop(next);
    }
}
