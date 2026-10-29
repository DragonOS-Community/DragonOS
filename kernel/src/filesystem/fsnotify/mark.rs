//! [`FsNotifyMark`]: a watch (group + inode + mask + wd) and its lifecycle management.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::filesystem::vfs::IndexNode;

use super::{
    adjust_total_watches, index_remove, FsNotifyGroup, FsNotifyObjectId, FsNotifyObjectState,
};
use crate::libs::mutex::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetireReason {
    Explicit,
    OneShot,
    ObjectDelete,
    Unmount,
    Shutdown,
}

/// Unique ownership of a mark's cleanup obligations.
pub(crate) struct RetireToken {
    mark: Arc<FsNotifyMark>,
    group: Arc<FsNotifyGroup>,
    reason: RetireReason,
}

/// A watch: links a group to an inode.
///
/// Lifecycle: `group.marks` holds a strong reference (pinning the watched
/// inode), while the global index holds a `Weak`.
/// Revoked on: `rm_watch`, `IN_DELETE_SELF`/`IN_UNMOUNT` being triggered, or
/// group destruction.
#[derive(Debug)]
pub struct FsNotifyMark {
    /// Watch descriptor, unique within a group.
    pub wd: i32,
    /// Owning group (weak reference, to avoid a reference cycle).
    pub group: Weak<FsNotifyGroup>,
    /// Strong reference: pins the inode for the duration of the watch
    /// (preventing eviction, so the InodeId is not reused).
    pub _inode: Arc<dyn IndexNode>,
    /// Keeps the shared per-object notification state alive without retaining
    /// a dentry.
    pub(crate) object_state: Option<Arc<FsNotifyObjectState>>,
    /// Captured once at watch creation; removal never performs metadata I/O.
    pub object_id: FsNotifyObjectId,
    /// Serializes dispatch with update/removal. This closes the one-shot and
    /// rm_watch race without a packed atomic state machine.
    pub dispatch_lock: Mutex<()>,
    pub active: AtomicBool,
    /// Subscribed mask (`IN_MASK_ADD` updates it concurrently; must be read
    /// atomically).
    pub mask: AtomicU32,
    /// `IN_ONESHOT`: revoke automatically after a single trigger.
    pub oneshot: AtomicBool,
    /// `IN_EXCL_UNLINK`: no longer emit events for an unlinked child.
    pub excl_unlink: AtomicBool,
}

impl FsNotifyMark {
    /// Get the identity of the watched inode: a (inode_id, dev_id) composite
    /// key.
    ///
    /// In multi-mount scenarios such as FUSE, the same inode number may be
    /// reused (e.g. FUSE_ROOT_ID=1). The (inode_id, dev_id) pair must be used
    /// to distinguish inodes on different mounts; otherwise a mark can be
    /// mismatched across mounts, causing event leaks or misjudging an existing
    /// watch.
    pub fn identity(&self) -> FsNotifyObjectId {
        self.object_id
    }

    pub(crate) fn watch_added(&self) {
        if let Some(state) = self.object_state.as_ref() {
            state.watch_added();
        } else if let Some(count) = self._inode.fsnotify_watch_count() {
            count.fetch_add(1, Ordering::Release);
        }
    }

    pub(crate) fn watch_removed(&self) {
        if let Some(state) = self.object_state.as_ref() {
            state.watch_removed();
        } else if let Some(count) = self._inode.fsnotify_watch_count() {
            let result = count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            });
            debug_assert!(result.is_ok(), "fsnotify inode watch count underflow");
        }
    }

    pub(crate) fn begin_retire(mark: &Arc<Self>, reason: RetireReason) -> Option<RetireToken> {
        let _dispatch = mark.dispatch_lock.lock();
        Self::begin_retire_locked(mark, reason)
    }

    /// Caller must hold `dispatch_lock`. Event delivery and one-shot/death
    /// retirement therefore form one indivisible dispatch transition.
    pub(crate) fn begin_retire_locked(
        mark: &Arc<Self>,
        reason: RetireReason,
    ) -> Option<RetireToken> {
        if !mark.active.load(Ordering::Acquire) {
            return None;
        }

        // An active mark is fully published and therefore must still belong to
        // a live group. Pin that group before transferring the unique cleanup
        // obligation to the token; shutdown may drop the last other strong
        // reference immediately after this dispatch critical section.
        let group = mark
            .group
            .upgrade()
            .expect("active fsnotify mark lost its group");
        if !mark.active.swap(false, Ordering::AcqRel) {
            return None;
        }
        Some(RetireToken {
            mark: mark.clone(),
            group,
            reason,
        })
    }
}

/// Revoke a mark: remove it from `group.marks` and the global index, and
/// maintain the global counter.
///
/// Called on `rm_watch`, `DELETE_SELF`/`UNMOUNT` dispatch, and group
/// destruction.
/// Note: the events lock is not taken, so this never blocks the read path
/// (separate lock families).
pub(crate) fn finish_retire(token: RetireToken) {
    let RetireToken {
        mark,
        group,
        reason,
    } = token;

    // Remove from group.marks (by pointer equality).
    let mut marks = group.marks.lock();
    let removed = marks
        .get(&mark.object_id)
        .is_some_and(|candidate| Arc::ptr_eq(candidate, &mark));
    if removed {
        marks.remove(&mark.object_id);
    }
    drop(marks);

    // The token, not group-map membership, owns every cleanup charge. This
    // remains correct when shutdown detached the map or a new mark replaced
    // the same object id.
    if reason != RetireReason::Shutdown {
        group.backend.notify_ignored(&group, &mark);
    }
    group.backend.free_mark(&mark);
    index_remove(&mark);
    mark.watch_removed();
    adjust_total_watches(-1);
}

/// Explicit rm_watch compatibility wrapper.
pub fn destroy_mark(mark: &Arc<FsNotifyMark>) -> bool {
    if let Some(token) = FsNotifyMark::begin_retire(mark, RetireReason::Explicit) {
        finish_retire(token);
        true
    } else {
        false
    }
}
