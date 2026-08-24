//! [`FsNotifyMark`]：一个 watch（group + inode + mask + wd）及其生命周期管理。

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

/// 一个 watch：连接 group 与 inode。
///
/// 生命周期：由 `group.marks` 持有强引用（pin 住被监听 inode），全局索引持 `Weak`。
/// 撤销时机：`rm_watch`、`IN_DELETE_SELF`/`IN_UNMOUNT` 触发、group 销毁。
#[derive(Debug)]
pub struct FsNotifyMark {
    /// watch descriptor，group 内唯一。
    pub wd: i32,
    /// 所属 group（弱引用，避免环引用）。
    pub group: Weak<FsNotifyGroup>,
    /// 强引用：watch 期间 pin 住 inode（防 evict，保证 InodeId 不复用）。
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
    /// 订阅 mask（`IN_MASK_ADD` 并发改，必须原子读）。
    pub mask: AtomicU32,
    /// `IN_ONESHOT`：触发一次后自动撤销。
    pub oneshot: AtomicBool,
    /// `IN_EXCL_UNLINK`：已 unlink 子项不再产生事件。
    pub excl_unlink: AtomicBool,
}

impl FsNotifyMark {
    /// 取被监听 inode 的标识：(inode_id, dev_id) 复合键。
    ///
    /// FUSE 等多挂载场景可能复用相同 inode 号（如 FUSE_ROOT_ID=1），
    /// 必须用 (inode_id, dev_id) 组合区分不同挂载上的 inode，否则会跨挂载
    /// 误匹配 mark，导致事件泄露或误判已有 watch。
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

/// 撤销一个 mark：从 group.marks、全局索引移除，并维护全局计数。
///
/// 在 `rm_watch`、`DELETE_SELF`/`UNMOUNT` dispatch、group 销毁时调用。
/// 注意：不取 events 锁，故与 read 路径互不阻塞（锁族分离）。
pub(crate) fn finish_retire(token: RetireToken) {
    let RetireToken {
        mark,
        group,
        reason,
    } = token;

    // 从 group.marks 移除（按指针相等）。
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
