//! Mounted-object fsnotify lifecycle and presence state.
//!
//! This module owns the two per-object concerns that must stay coherent across
//! hard-link aliases: final-link deletion and the object-local mark snapshot.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::filesystem::vfs::{InodeId, LinkRemovalOutcome};
use crate::libs::mutex::Mutex;

use super::{notify_object_delete, MarkList};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FsNotifyObjectId {
    pub superblock: usize,
    pub inode: InodeId,
    pub generation: u64,
}

#[derive(Debug)]
pub(crate) struct FsNotifyDeleteState {
    pending: bool,
    committed: bool,
    link_epoch: usize,
}

#[derive(Debug)]
pub(crate) struct FsNotifyObjectState {
    pub(crate) delete: Mutex<FsNotifyDeleteState>,
    watches: AtomicUsize,
    pub(super) marks: Mutex<Option<MarkList>>,
    presence: Arc<MountedFsNotifyPresence>,
    is_dir: bool,
}

// SAFETY: mutable fields are protected by `Mutex` or atomics. The mark list
// contains only Weak references and is cloned under `marks` before dispatch.
// These explicit impls break the recursive auto-trait proof
// object-state -> Weak<mark> -> inode -> mount -> object-state; they do not
// bypass any synchronization requirement.
unsafe impl Send for FsNotifyObjectState {}
unsafe impl Sync for FsNotifyObjectState {}

#[derive(Debug)]
pub(crate) struct MountedFsNotifyPresence {
    watches: AtomicUsize,
    directory_watches: AtomicUsize,
    interest_epoch: AtomicUsize,
}

impl Default for MountedFsNotifyPresence {
    fn default() -> Self {
        Self {
            watches: AtomicUsize::new(0),
            directory_watches: AtomicUsize::new(0),
            // Dentry negative-interest caches use zero as "not validated".
            interest_epoch: AtomicUsize::new(1),
        }
    }
}

impl MountedFsNotifyPresence {
    pub(crate) fn has_watches(&self) -> bool {
        self.watches.load(Ordering::Acquire) != 0
    }

    pub(crate) fn has_directory_watches(&self) -> bool {
        self.directory_watches.load(Ordering::Acquire) != 0
    }

    pub(crate) fn interest_epoch(&self) -> usize {
        self.interest_epoch.load(Ordering::Acquire)
    }

    fn advance_interest_epoch(&self) {
        // Saturation permanently disables negative caching at MAX. This is a
        // safe slow-path fallback and avoids accepting an ancient cache entry
        // after a theoretical counter wrap.
        let _ = self
            .interest_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                epoch.checked_add(1)
            });
    }
}

impl FsNotifyObjectState {
    pub(crate) fn new(
        delete_pending: bool,
        link_epoch: usize,
        presence: Arc<MountedFsNotifyPresence>,
        is_dir: bool,
    ) -> Self {
        Self {
            delete: Mutex::new(FsNotifyDeleteState::new(delete_pending, link_epoch)),
            watches: AtomicUsize::new(0),
            marks: Mutex::new(None),
            presence,
            is_dir,
        }
    }

    pub(crate) fn has_watches(&self) -> bool {
        self.watches.load(Ordering::Acquire) != 0
    }

    pub(crate) fn watch_added(&self) {
        let previous = self.watches.fetch_add(1, Ordering::Release);
        self.presence.watches.fetch_add(1, Ordering::Release);
        if self.is_dir {
            self.presence
                .directory_watches
                .fetch_add(1, Ordering::Release);
        }
        if previous == 0 {
            // Publish the object and superblock counts before invalidating
            // dentry negative-interest caches. Active is published later.
            self.presence.advance_interest_epoch();
        }
    }

    pub(crate) fn watch_removed(&self) {
        let object_result =
            self.watches
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                });
        debug_assert!(
            object_result.is_ok(),
            "fsnotify object watch count underflow"
        );
        let result =
            self.presence
                .watches
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                });
        debug_assert!(result.is_ok(), "mounted fsnotify watch count underflow");
        if self.is_dir {
            let result = self.presence.directory_watches.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |count| count.checked_sub(1),
            );
            debug_assert!(
                result.is_ok(),
                "mounted fsnotify directory watch count underflow"
            );
        }
        if object_result.is_ok_and(|previous| previous == 1) {
            // Retirement has already made the mark inactive and removed it
            // from the dispatch index. Invalidate negative caches only after
            // all presence counts describe the new state.
            self.presence.advance_interest_epoch();
        }
    }

    pub(super) fn mark_snapshot(&self) -> Option<MarkList> {
        self.marks.lock().clone()
    }
}

impl FsNotifyDeleteState {
    pub(crate) fn new(pending: bool, link_epoch: usize) -> Self {
        Self {
            pending,
            committed: false,
            link_epoch,
        }
    }

    pub(crate) fn committed(&self) -> bool {
        self.committed
    }
}

pub(crate) fn note_link_removed(
    state: &mut FsNotifyDeleteState,
    outcome: LinkRemovalOutcome,
    link_epoch: usize,
) {
    if link_epoch < state.link_epoch {
        return;
    }
    state.link_epoch = link_epoch;
    match outcome {
        LinkRemovalOutcome::StillLinked => {
            state.pending = false;
            state.committed = false;
        }
        LinkRemovalOutcome::LastLink => state.pending = true,
    }
}

pub(crate) fn note_link_added(state: &mut FsNotifyDeleteState, link_epoch: usize) {
    if link_epoch < state.link_epoch {
        return;
    }
    state.link_epoch = link_epoch;
    state.pending = false;
    state.committed = false;
}

/// Commit final object deletion at the irreversible dentry detach boundary.
pub(crate) fn notify_dentry_detach(
    id: FsNotifyObjectId,
    object: &FsNotifyObjectState,
    state: &mut FsNotifyDeleteState,
) {
    if !state.pending {
        return;
    }
    state.pending = false;
    state.committed = true;
    notify_object_delete(id, object);
}
