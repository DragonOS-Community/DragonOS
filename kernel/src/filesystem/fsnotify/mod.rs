//! Unified layer for filesystem event notification (fsnotify).
//!
//! This module is the decoupling layer between the VFS write-path hooks and the concrete backends (currently only inotify).
//! The VFS hooks only call [`fsnotify`]. Watches on mounted filesystems use object-local snapshots,
//! while unmounted objects use the global fallback index; events are dispatched after an immutable snapshot is acquired.
//!
//! Design principles (see `docs/kernel/filesystem/inotify.md` §0/§3):
//! - `fsnotify()` is best-effort and never affects the syscall return value;
//! - `fsnotify()` internally takes only this layer's spinlocks and the group queue lock, and never calls back into `IndexNode` write methods;
//! - Lock order: the object snapshot lock is only used for clone/publish; after it is released we enter the mark and group queues, never in reverse.

pub mod group;
pub mod mark;
mod object;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use hashbrown::HashMap;

use crate::filesystem::vfs::{mount::MountFSInode, FileType, IndexNode};
use crate::libs::casting::DowncastArc;
use crate::libs::mutex::Mutex;
use system_error::SystemError;

pub use group::FsNotifyGroup;
pub use mark::FsNotifyMark;
use mark::{finish_retire, RetireReason};
pub use object::FsNotifyObjectId;
pub(crate) use object::{
    note_link_added, note_link_removed, notify_dentry_detach, FsNotifyObjectState,
    MountedFsNotifyPresence,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueResult {
    Queued,
    Merged,
    DroppedQueueFull,
    AllocationFailed,
    Filtered,
}

#[derive(Clone, Debug)]
pub struct FsNotifyTarget {
    pub id: FsNotifyObjectId,
    pub is_dir: bool,
    pub disconnected: bool,
    pub watched: bool,
    pub(crate) object_state: Option<Arc<FsNotifyObjectState>>,
}

pub fn target_for_inode(inode: &Arc<dyn IndexNode>) -> Result<FsNotifyTarget, SystemError> {
    if let Some(mounted) = inode.clone().downcast_arc::<MountFSInode>() {
        let (superblock, ino, generation, file_type, disconnected, watched) =
            mounted.fsnotify_target();
        return Ok(FsNotifyTarget {
            id: FsNotifyObjectId {
                superblock,
                inode: ino,
                generation,
            },
            is_dir: file_type == FileType::Dir,
            disconnected,
            watched,
            object_state: mounted.fsnotify_object_state(),
        });
    }
    let md = inode.metadata()?;
    Ok(FsNotifyTarget {
        id: FsNotifyObjectId {
            superblock: md.dev_id,
            inode: md.inode_id,
            generation: inode.inode_generation(),
        },
        is_dir: md.file_type == FileType::Dir,
        disconnected: md.nlinks == 0,
        // Anonymous inodes may expose a local presence hint. Unknown inode
        // implementations remain conservative so this optimization cannot
        // suppress notifications.
        watched: inode
            .fsnotify_watch_count()
            .map(|count| count.load(Ordering::Acquire) != 0)
            .unwrap_or(true),
        object_state: None,
    })
}

/// Resolve an event target after consulting an inode-local watch hint.
///
/// Identity lookups used to add a watch must always resolve the inode. Event
/// delivery may skip that work when an anonymous inode authoritatively reports
/// that it has no marks; implementations without a hint remain conservative.
fn target_for_event(inode: &Arc<dyn IndexNode>) -> Result<Option<FsNotifyTarget>, SystemError> {
    if inode
        .fsnotify_watch_count()
        .is_some_and(|count| count.load(Ordering::Acquire) == 0)
    {
        return Ok(None);
    }
    target_for_inode(inode).map(Some)
}

pub fn canonical_inode(inode: Arc<dyn IndexNode>) -> Arc<dyn IndexNode> {
    inode
        .clone()
        .downcast_arc::<MountFSInode>()
        .map(|mounted| mounted.underlying_inode())
        .unwrap_or(inode)
}

/// Notify both a pathname's current parent entry and the inode itself. This is
/// used by content/attribute operations whose Linux events are visible to both
/// a directory watch and a direct inode watch.
pub fn fsnotify_inode(mask: FsEvent, inode: &Arc<dyn IndexNode>) {
    if !has_any_watch() {
        return;
    }
    if let Some(mounted) = inode.clone().downcast_arc::<MountFSInode>() {
        let Some((child, parent)) = mounted.fsnotify_snapshot() else {
            return;
        };
        if let Some((parent, name)) = parent.as_ref() {
            return fsnotify_targets(
                mask,
                Some((parent, name.0.as_str())),
                Some(&child),
                0,
                false,
            );
        }
        return fsnotify_targets(mask, None, Some(&child), 0, false);
    }
    fsnotify_with_data(mask, None, Some(inode), 0, false);
}

// Event mask: corresponds to the Linux kernel `FS_*` events; its bits match the user-space `IN_*` bits exactly,
// so it can be used directly as a user-space mask (only `ISDIR` is set on demand by dispatch).
//
// Reference: Linux `include/uapi/linux/inotify.h`, `include/linux/fsnotify_backend.h`.
bitflags::bitflags! {
    pub struct FsEvent: u32 {
        const ACCESS       = 0x00000001; // IN_ACCESS
        const MODIFY       = 0x00000002; // IN_MODIFY
        const ATTRIB       = 0x00000004; // IN_ATTRIB
        const CLOSE_WRITE  = 0x00000008; // IN_CLOSE_WRITE
        const CLOSE_NOWRITE= 0x00000010; // IN_CLOSE_NOWRITE
        const OPEN         = 0x00000020; // IN_OPEN
        const MOVED_FROM   = 0x00000040; // IN_MOVED_FROM
        const MOVED_TO     = 0x00000080; // IN_MOVED_TO
        const CREATE       = 0x00000100; // IN_CREATE
        const DELETE       = 0x00000200; // IN_DELETE
        const DELETE_SELF  = 0x00000400; // IN_DELETE_SELF
        const MOVE_SELF    = 0x00000800; // IN_MOVE_SELF
        const UNMOUNT      = 0x00002000; // IN_UNMOUNT (filesystem unmount)
        const Q_OVERFLOW   = 0x00004000; // IN_Q_OVERFLOW (queue overflow)
        const IN_IGNORED   = 0x00008000; // watch was revoked (inode deleted/unmounted)
        const ISDIR        = 0x40000000; // event target is a directory (set by dispatch)
    }
}

/// Backend interface (minimal abstraction).
///
/// The fsnotify layer calls the concrete backend through this trait, keeping the VFS → fsnotify → inotify dependency one-directional.
pub trait FsNotifyBackend: Send + Sync + core::fmt::Debug {
    /// Handle an event: format, (optionally) merge, enqueue, and wake waiters.
    fn handle_event(
        &self,
        group: &FsNotifyGroup,
        mark: &FsNotifyMark,
        mask: FsEvent,
        name: Option<&str>,
        cookie: u32,
    ) -> EnqueueResult;
    /// Remove the mark from the backend's internal structures (e.g. the wd table) when it is destroyed.
    fn free_mark(&self, mark: &FsNotifyMark);
    /// Deliver an IN_IGNORED event to the consumer when the mark is revoked (rm_watch/oneshot/DELETE_SELF/UNMOUNT).
    /// This method is not called on the fd close (shutdown) path.
    fn notify_ignored(&self, group: &FsNotifyGroup, mark: &FsNotifyMark);
    /// For poll: whether the queue is non-empty.
    fn queue_nonempty(&self) -> bool;
}

/// Global watch count: zero the vast majority of the time. The first gate of `fsnotify` —
/// zero lock overhead when there are no watches (mirrors Linux's `i_fsnotify_mask` fast skip).
static TOTAL_WATCHES: AtomicUsize = AtomicUsize::new(0);

/// move event cookie allocator: one per rename, shared by FROM/TO.
/// 0 means "no move", so it starts at 1 and skips 0 on wrap-around.
static NEXT_COOKIE: AtomicU32 = AtomicU32::new(1);

/// Take a new non-zero move cookie.
pub fn next_cookie() -> u32 {
    loop {
        let c = NEXT_COOKIE.fetch_add(1, Ordering::Relaxed);
        if c != 0 {
            return c;
        }
    }
}
// Global mark index: uses a composite `(InodeId, dev_id)` key to reverse-lookup "all marks attached to that inode".
//
// The composite key is required: multiple FUSE mounts reuse the same inode number (e.g. FUSE_ROOT_ID=1), and a bare InodeId key
// would mismatch across mounts, causing event leaks / misjudging existing watches.
// Stores `Weak<FsNotifyMark>`: the group owns the mark (strong reference), and the index only performs lookups without preventing reclamation.
// Dead references whose `Weak::upgrade()` fails during dispatch are lazily pruned.
type MarkList = Arc<Vec<alloc::sync::Weak<FsNotifyMark>>>;
type MarkIndex = HashMap<FsNotifyObjectId, MarkList>;

lazy_static::lazy_static! {
    static ref FSNOTIFY_MARKS: Mutex<MarkIndex> = Mutex::new(HashMap::new());
}

pub(crate) fn notify_object_delete(_id: FsNotifyObjectId, object: &FsNotifyObjectState) {
    if !object.has_watches() {
        return;
    }
    let marks = object.mark_snapshot();
    for mark in marks
        .iter()
        .flat_map(|entries| entries.iter())
        .filter_map(|entry| entry.upgrade())
    {
        let guard = mark.dispatch_lock.lock();
        if !mark.active.load(Ordering::Acquire) {
            continue;
        }
        if let Some(group) = mark.group.upgrade() {
            group
                .backend
                .handle_event(&group, &mark, FsEvent::DELETE_SELF, None, 0);
        }
        let token = FsNotifyMark::begin_retire_locked(&mark, RetireReason::ObjectDelete);
        drop(guard);
        if let Some(token) = token {
            finish_retire(token);
        }
    }
}

fn notify_unmount_mark(mark: &Arc<FsNotifyMark>) {
    let guard = mark.dispatch_lock.lock();
    if !mark.active.load(Ordering::Acquire) {
        return;
    }
    if let Some(group) = mark.group.upgrade() {
        group
            .backend
            .handle_event(&group, mark, FsEvent::UNMOUNT, None, 0);
    }
    let token = FsNotifyMark::begin_retire_locked(mark, RetireReason::Unmount);
    drop(guard);
    if let Some(token) = token {
        finish_retire(token);
    }
}

pub(crate) fn notify_unmount_object(object: &FsNotifyObjectState) {
    let marks = object.mark_snapshot();
    for mark in marks
        .iter()
        .flat_map(|entries| entries.iter())
        .filter_map(|entry| entry.upgrade())
    {
        notify_unmount_mark(&mark);
    }
}

/// Record a watch-count change (add +1, revoke -1).
pub(crate) fn adjust_total_watches(delta: i32) {
    if delta >= 0 {
        TOTAL_WATCHES.fetch_add(delta as usize, Ordering::Release);
    } else {
        TOTAL_WATCHES.fetch_sub((-delta) as usize, Ordering::AcqRel);
    }
}

/// Whether any inotify watch exists in the system. Lets the VFS hot paths (open/read/write/close) short-circuit cheaply:
/// with no watches, parent resolution and the fsnotify call are skipped entirely (zero overhead).
pub fn has_any_watch() -> bool {
    TOTAL_WATCHES.load(Ordering::Acquire) != 0
}

pub(crate) fn index_add(mark: &Arc<FsNotifyMark>) -> Result<(), SystemError> {
    if let Some(object) = mark.object_state.as_ref() {
        return object_index_add(object, mark);
    }
    let key = mark.identity();
    loop {
        let old = FSNOTIFY_MARKS.lock().get(&key).cloned();
        let live = old
            .iter()
            .flat_map(|entries| entries.iter())
            .filter(|entry| entry.strong_count() != 0)
            .count();
        let mut next = Vec::new();
        next.try_reserve_exact(live.saturating_add(1))
            .map_err(|_| SystemError::ENOMEM)?;
        next.extend(
            old.iter()
                .flat_map(|entries| entries.iter())
                .filter(|entry| entry.strong_count() != 0)
                .cloned(),
        );
        next.push(Arc::downgrade(mark));
        let next = Arc::try_new(next).map_err(|_| SystemError::ENOMEM)?;

        let mut idx = FSNOTIFY_MARKS.lock();
        let unchanged = match (idx.get(&key), old.as_ref()) {
            (Some(current), Some(old)) => Arc::ptr_eq(current, old),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            continue;
        }
        idx.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        idx.insert(key, next);
        return Ok(());
    }
}
/// Remove a mark from the global index (matched by pointer equality, called on rm_watch / revoke).
pub(crate) fn index_remove(mark: &FsNotifyMark) {
    if let Some(object) = mark.object_state.as_ref() {
        object_index_remove(object, mark);
        return;
    }
    let key = mark.identity();
    let self_ptr = mark as *const FsNotifyMark;
    loop {
        let Some(old) = FSNOTIFY_MARKS.lock().get(&key).cloned() else {
            return;
        };

        // Removing the last live mark must not depend on allocating a COW
        // replacement.  Besides being the common case, this prevents an OOM
        // during rm_watch/shutdown from retaining an otherwise empty index key
        // indefinitely.  A concurrent publisher replaces `old`, so the
        // pointer check below makes the allocation-free removal retry safely.
        let has_survivor = old.iter().any(|entry| {
            entry
                .upgrade()
                .is_some_and(|arc| !core::ptr::eq(Arc::as_ptr(&arc), self_ptr))
        });
        if !has_survivor {
            let mut idx = FSNOTIFY_MARKS.lock();
            if !idx
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &old))
            {
                continue;
            }
            idx.remove(&key);
            return;
        }

        let mut compact = Vec::new();
        if compact.try_reserve_exact(old.len()).is_err() {
            return;
        }
        compact.extend(old.iter().filter_map(|entry| {
            entry.upgrade().and_then(|arc| {
                (!core::ptr::eq(Arc::as_ptr(&arc), self_ptr)).then(|| Arc::downgrade(&arc))
            })
        }));
        let replacement = if compact.is_empty() {
            None
        } else {
            match Arc::try_new(compact) {
                Ok(entries) => Some(entries),
                Err(_) => return,
            }
        };
        let mut idx = FSNOTIFY_MARKS.lock();
        if !idx
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &old))
        {
            continue;
        }
        match replacement {
            Some(entries) => {
                idx.insert(key, entries);
            }
            None => {
                idx.remove(&key);
            }
        }
        return;
    }
}

fn object_index_add(
    object: &FsNotifyObjectState,
    mark: &Arc<FsNotifyMark>,
) -> Result<(), SystemError> {
    loop {
        let old = object.mark_snapshot();
        let live = old
            .iter()
            .flat_map(|entries| entries.iter())
            .filter(|entry| entry.strong_count() != 0)
            .count();
        let mut next = Vec::new();
        next.try_reserve_exact(live.saturating_add(1))
            .map_err(|_| SystemError::ENOMEM)?;
        next.extend(
            old.iter()
                .flat_map(|entries| entries.iter())
                .filter(|entry| entry.strong_count() != 0)
                .cloned(),
        );
        next.push(Arc::downgrade(mark));
        let next = Arc::try_new(next).map_err(|_| SystemError::ENOMEM)?;

        let mut current = object.marks.lock();
        let unchanged = match (current.as_ref(), old.as_ref()) {
            (Some(current), Some(old)) => Arc::ptr_eq(current, old),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            *current = Some(next);
            return Ok(());
        }
    }
}

fn object_index_remove(object: &FsNotifyObjectState, mark: &FsNotifyMark) {
    let self_ptr = mark as *const FsNotifyMark;
    loop {
        let Some(old) = object.mark_snapshot() else {
            return;
        };
        let has_survivor = old.iter().any(|entry| {
            entry
                .upgrade()
                .is_some_and(|arc| !core::ptr::eq(Arc::as_ptr(&arc), self_ptr))
        });
        if !has_survivor {
            let mut current = object.marks.lock();
            if !current
                .as_ref()
                .is_some_and(|entries| Arc::ptr_eq(entries, &old))
            {
                continue;
            }
            *current = None;
            return;
        }

        let mut compact = Vec::new();
        if compact.try_reserve_exact(old.len()).is_err() {
            return;
        }
        compact.extend(old.iter().filter_map(|entry| {
            entry.upgrade().and_then(|arc| {
                (!core::ptr::eq(Arc::as_ptr(&arc), self_ptr)).then(|| Arc::downgrade(&arc))
            })
        }));
        let replacement = if compact.is_empty() {
            None
        } else {
            match Arc::try_new(compact) {
                Ok(entries) => Some(entries),
                Err(_) => return,
            }
        };
        let mut current = object.marks.lock();
        if !current
            .as_ref()
            .is_some_and(|entries| Arc::ptr_eq(entries, &old))
        {
            continue;
        }
        *current = replacement;
        return;
    }
}

/// Unified event delivery entry point. Called **after a VFS operation succeeds**, best-effort, and never affects the caller's return value.
///
/// - `parent`: for child events (CREATE/DELETE/MOVED_*), pass `(parent directory inode, child name)`;
/// - `child`: for self events (DELETE_SELF/MOVE_SELF/MODIFY/CLOSE/OPEN/ATTRIB), pass the target inode;
/// - Both may be non-null simultaneously (e.g. unlink: the parent directory gets `IN_DELETE`, the child gets `IN_DELETE_SELF`).
///
/// # Safety guarantees
/// Internally it only reads `inode.metadata()` (inode is alive, metadata is read-only without locking), and calls no write methods,
/// avoiding re-entry under the VFS/File locks held by the caller.
pub fn fsnotify(
    mask: FsEvent,
    parent: Option<(&Arc<dyn IndexNode>, &str)>,
    child: Option<&Arc<dyn IndexNode>>,
    cookie: u32,
) {
    fsnotify_with_data(mask, parent, child, cookie, true)
}

fn fsnotify_with_data(
    mask: FsEvent,
    parent: Option<(&Arc<dyn IndexNode>, &str)>,
    child: Option<&Arc<dyn IndexNode>>,
    cookie: u32,
    path_event: bool,
) {
    // ① Fast path: no watches in the system → return immediately (zero cost on the read/write/close hot path).
    if TOTAL_WATCHES.load(Ordering::Relaxed) == 0 {
        return;
    }

    // ② Prefetch inode metadata (inode is alive, metadata is read-only without locking, safe).
    // The "subject" of the event is the child (the object created/deleted/moved/modified); IN_ISDIR is determined by whether the subject
    // is a directory, applying uniformly to parent and child watches. If there is no child (the degenerate case of only a parent-directory self event),
    // ISDIR is not set.
    // Use the (inode_id, dev_id) composite key: multiple FUSE mounts reuse the same inode number, so dev_id must be added to disambiguate.
    let child_target = child.and_then(|inode| target_for_event(inode).ok().flatten());
    let parent_target = parent.and_then(|(inode, _)| target_for_event(inode).ok().flatten());
    fsnotify_targets(
        mask,
        parent_target.as_ref().zip(parent.map(|(_, name)| name)),
        child_target.as_ref(),
        cookie,
        path_event,
    );
}

/// Metadata-I/O-free dispatch entry for callers that already hold a coherent
/// dentry snapshot.
pub(crate) fn fsnotify_targets(
    mask: FsEvent,
    parent: Option<(&FsNotifyTarget, &str)>,
    child: Option<&FsNotifyTarget>,
    cookie: u32,
    path_event: bool,
) {
    if TOTAL_WATCHES.load(Ordering::Relaxed) == 0 {
        return;
    }
    let (child_key, event_is_dir, child_unlinked) = child
        .map(|target| {
            (
                target.watched.then_some(target.id),
                target.is_dir,
                target.disconnected,
            )
        })
        .unwrap_or((None, false, false));
    let parent_key = parent.and_then(|(target, _)| target.watched.then_some(target.id));

    if child_key.is_none() && parent_key.is_none() {
        return;
    }

    // ③ Collect candidate mark snapshots: the critical section only does a hash lookup (held only briefly), no backend work.
    // (mark strong reference, name, is_parent)
    let local_parent = parent.and_then(|(target, _)| target.object_state.as_ref());
    let local_child = child.and_then(|target| target.object_state.as_ref());
    let parent_marks = local_parent
        .and_then(|object| object.mark_snapshot())
        .or_else(|| {
            parent_key.and_then(|key| {
                (local_parent.is_none())
                    .then(|| FSNOTIFY_MARKS.lock().get(&key).cloned())
                    .flatten()
            })
        });
    let child_marks = local_child
        .and_then(|object| object.mark_snapshot())
        .or_else(|| {
            child_key.and_then(|key| {
                (local_child.is_none())
                    .then(|| FSNOTIFY_MARKS.lock().get(&key).cloned())
                    .flatten()
            })
        });
    // Note: dead Weak refs that fail to upgrade within the lock are skipped; lazy cleanup is left to index_remove.

    // Event routing (Linux model):
    // - Namespace events CREATE/DELETE/MOVED_FROM/MOVED_TO: only the parent directory watch receives them (with name);
    // - Self events DELETE_SELF/MOVE_SELF: only the child's own watch receives them;
    // - Content events ACCESS/MODIFY/ATTRIB/CLOSE_*/OPEN: both the parent directory watch (with name) and the child's own watch
    //   receive them — so a "watched directory" receives events for child files being read/written/opened/closed/attribute-changed (inotify's #1 use case).
    // A single fsnotify call can notify both the parent directory and the child (e.g. unlink: parent gets IN_DELETE, child gets IN_DELETE_SELF).
    let self_only = FsEvent::DELETE_SELF | FsEvent::MOVE_SELF;
    let parent_only = FsEvent::CREATE | FsEvent::DELETE | FsEvent::MOVED_FROM | FsEvent::MOVED_TO;
    // Content events (suppressed by IN_EXCL_UNLINK).
    let content_type = FsEvent::MODIFY
        | FsEvent::ACCESS
        | FsEvent::CLOSE_WRITE
        | FsEvent::CLOSE_NOWRITE
        | FsEvent::OPEN;

    // ④ Deliver outside the lock.
    let parent_name = parent.map(|(_, name)| name);
    let candidates = parent_marks
        .iter()
        .flat_map(|entries| entries.iter())
        .filter_map(|entry| entry.upgrade().map(|mark| (mark, parent_name, true)))
        .chain(
            child_marks
                .iter()
                .flat_map(|entries| entries.iter())
                .filter_map(|entry| entry.upgrade().map(|mark| (mark, None, false))),
        );
    for (mark, name, is_parent) in candidates {
        let dispatch_guard = mark.dispatch_lock.lock();
        if !mark.active.load(Ordering::Acquire) {
            continue;
        }
        // The parent mark receives everything except self_only; the self mark receives everything except parent_only.
        let routed = if is_parent {
            mask & !self_only
        } else {
            mask & !parent_only
        };
        if routed.is_empty() {
            continue;
        }

        // Inode death events (DELETE_SELF/UNMOUNT): the mark must be revoked regardless of whether the watch is subscribed,
        // and IN_IGNORED must be delivered (enqueued unconditionally by destroy_mark); otherwise the watch leaks a strong reference.
        let inode_death =
            routed.contains(FsEvent::DELETE_SELF) || routed.contains(FsEvent::UNMOUNT);

        let subscribed = mark.mask.load(Ordering::Relaxed);
        let mask_matches = (subscribed & routed.bits()) != 0;

        // Non-inode-death events: skip when not subscribed or suppressed by EXCL_UNLINK.
        if !inode_death {
            if !mask_matches {
                continue;
            }
            // IN_EXCL_UNLINK: suppress path-data content events for an
            // unlinked dentry on both parent and direct inode marks. Dentry
            // data events such as ftruncate remain visible, matching Linux.
            if mark.excl_unlink.load(Ordering::Relaxed)
                && path_event
                && routed.intersects(content_type)
                && child_unlinked
            {
                continue;
            }
        }

        // dispatch sets ISDIR (when the subject is a directory).
        let mut delivered = routed;
        if event_is_dir && !routed.intersects(self_only) {
            delivered |= FsEvent::ISDIR;
        }

        let group = mark.group.upgrade();
        let enqueue_result = if let Some(group) = group.as_ref() {
            group
                .backend
                .handle_event(group, &mark, delivered, name, cookie)
        } else {
            EnqueueResult::Filtered
        };

        // Revoke: on inode death (unconditional) or oneshot (revoke after a single trigger when the subscription matched).
        let consumes_oneshot = matches!(
            enqueue_result,
            EnqueueResult::Queued | EnqueueResult::Merged | EnqueueResult::DroppedQueueFull
        );
        let retire_reason = if inode_death {
            Some(RetireReason::ObjectDelete)
        } else if mark.oneshot.load(Ordering::Relaxed) && consumes_oneshot {
            Some(RetireReason::OneShot)
        } else {
            None
        };
        let token =
            retire_reason.and_then(|reason| FsNotifyMark::begin_retire_locked(&mark, reason));
        drop(dispatch_guard);
        if let Some(token) = token {
            finish_retire(token);
        }
    }
}
