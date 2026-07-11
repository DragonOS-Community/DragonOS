use alloc::sync::Arc;
use system_error::SystemError;

use super::IndexNode;
use crate::libs::spinlock::SpinLock;

/// Monotonic completion boundary for filesystem eviction requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EvictionEpoch(u64);

impl EvictionEpoch {
    pub const EMPTY: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// The semantic owner of an inode lifetime pin.
///
/// Unlike an `Arc`, these pins describe references that filesystem eviction
/// must wait for. They must be attached to the canonical inode lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeRetentionKind {
    OpenFileDescription,
    Cache,
    AsyncWork,
    Operation,
}

impl InodeRetentionKind {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        match self {
            Self::OpenFileDescription => 0,
            Self::Cache => 1,
            Self::AsyncWork => 2,
            Self::Operation => 3,
        }
    }
}

/// Filesystem-embeddable semantic retention accounting.
///
/// A single lock linearizes retain against the final release. This avoids the
/// false-zero window that independent atomic kind/total counters would create
/// when a new owner races the previous owner's release.
#[derive(Debug)]
pub struct InodeRetentionState {
    inner: SpinLock<InodeRetentionInner>,
}

#[derive(Debug)]
struct InodeRetentionInner {
    counts: [usize; InodeRetentionKind::COUNT],
    admitting: bool,
}

impl Default for InodeRetentionState {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeRetentionState {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(InodeRetentionInner {
                counts: [0; InodeRetentionKind::COUNT],
                admitting: true,
            }),
        }
    }

    pub fn retain(&self, kind: InodeRetentionKind) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        if !inner.admitting {
            return Err(SystemError::EBUSY);
        }
        inner.counts[kind.index()] = inner.counts[kind.index()]
            .checked_add(1)
            .expect("inode semantic retention overflow");
        Ok(())
    }

    /// Release one owner and return whether all semantic owners are now gone.
    ///
    /// A `true` result is only an eviction notification edge. The filesystem
    /// must still check link/cache state and publish a one-shot request.
    pub fn release(&self, kind: InodeRetentionKind) -> bool {
        let mut inner = self.inner.lock();
        let count = &mut inner.counts[kind.index()];
        assert!(*count != 0, "inode semantic retention underflow");
        *count -= 1;
        inner.counts.iter().all(|count| *count == 0)
    }

    /// Atomically stop new retention admission if no semantic owner remains.
    pub fn try_begin_freeing(&self) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        if !inner.admitting || inner.counts.iter().any(|count| *count != 0) {
            return Err(SystemError::EBUSY);
        }
        inner.admitting = false;
        Ok(())
    }

    /// Reopen admission after a filesystem freeing transaction is aborted.
    pub fn abort_freeing(&self) {
        self.inner.lock().admitting = true;
    }
}

/// Exactly-once RAII pairing for `IndexNode::retain`/`release`.
///
/// Dropping this guard must never perform filesystem I/O. A filesystem may
/// only publish a deferred eviction request from `release`; fallible work is
/// completed through its explicit eviction drain path.
#[derive(Debug)]
pub struct InodeRetentionGuard {
    inode: Arc<dyn IndexNode>,
    kind: InodeRetentionKind,
}

impl InodeRetentionGuard {
    pub fn new(inode: Arc<dyn IndexNode>, kind: InodeRetentionKind) -> Result<Self, SystemError> {
        inode.retain(kind)?;
        Ok(Self { inode, kind })
    }
}

impl Drop for InodeRetentionGuard {
    fn drop(&mut self) {
        self.inode.release(self.kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    struct MockInode {
        retention: InodeRetentionState,
        eviction_notifications: AtomicUsize,
    }

    impl MockInode {
        fn new() -> Self {
            Self {
                retention: InodeRetentionState::new(),
                eviction_notifications: AtomicUsize::new(0),
            }
        }

        fn retain(&self, kind: InodeRetentionKind) {
            self.retention.retain(kind).unwrap();
        }

        fn release(&self, kind: InodeRetentionKind) {
            if self.retention.release(kind) {
                self.eviction_notifications.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn retention_kinds_release_exactly_once_at_final_edge() {
        let inode = MockInode::new();
        inode.retain(InodeRetentionKind::OpenFileDescription);
        inode.retain(InodeRetentionKind::AsyncWork);

        inode.release(InodeRetentionKind::OpenFileDescription);
        assert_eq!(inode.eviction_notifications.load(Ordering::Relaxed), 0);
        inode.release(InodeRetentionKind::AsyncWork);
        assert_eq!(inode.eviction_notifications.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[should_panic(expected = "inode semantic retention underflow")]
    fn retention_underflow_is_not_hidden() {
        InodeRetentionState::new().release(InodeRetentionKind::Cache);
    }

    #[test]
    fn freeing_admission_is_linearized_with_retention() {
        let state = InodeRetentionState::new();
        state
            .retain(InodeRetentionKind::OpenFileDescription)
            .unwrap();
        assert_eq!(state.try_begin_freeing(), Err(SystemError::EBUSY));
        assert!(state.release(InodeRetentionKind::OpenFileDescription));
        state.try_begin_freeing().unwrap();
        assert_eq!(
            state.retain(InodeRetentionKind::Operation),
            Err(SystemError::EBUSY)
        );
        state.abort_freeing();
        state.retain(InodeRetentionKind::Operation).unwrap();
        assert!(state.release(InodeRetentionKind::Operation));
    }
}
