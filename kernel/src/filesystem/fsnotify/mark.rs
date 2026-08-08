//! [`FsNotifyMark`]：一个 watch（group + inode + mask + wd）及其生命周期管理。

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU32};

use crate::filesystem::vfs::{IndexNode, InodeId};

use super::{adjust_total_watches, index_remove, FsNotifyGroup};

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
    pub inode: Arc<dyn IndexNode>,
    /// 订阅 mask（`IN_MASK_ADD` 并发改，必须原子读）。
    pub mask: AtomicU32,
    /// `IN_ONESHOT`：触发一次后自动撤销。
    pub oneshot: AtomicBool,
    /// `IN_EXCL_UNLINK`：已 unlink 子项不再产生事件。
    pub excl_unlink: bool,
}

impl FsNotifyMark {
    /// 取被监听 inode 的标识：(inode_id, dev_id) 复合键。
    ///
    /// FUSE 等多挂载场景可能复用相同 inode 号（如 FUSE_ROOT_ID=1），
    /// 必须用 (inode_id, dev_id) 组合区分不同挂载上的 inode，否则会跨挂载
    /// 误匹配 mark，导致事件泄露或误判已有 watch。
    pub fn identity(&self) -> (InodeId, usize) {
        self.inode
            .metadata()
            .map(|m| (m.inode_id, m.dev_id))
            .unwrap_or((InodeId::new(0), 0))
    }

}

/// 撤销一个 mark：从 group.marks、全局索引移除，并维护全局计数。
///
/// 在 `rm_watch`、`DELETE_SELF`/`UNMOUNT` dispatch、group 销毁时调用。
/// 注意：不取 events 锁，故与 read 路径互不阻塞（锁族分离）。
pub fn destroy_mark(mark: &Arc<FsNotifyMark>) {
    let Some(group) = mark.group.upgrade() else {
        // group 已销毁，mark 仅可能残留在 snapshot 中；直接清索引即可。
        index_remove(mark);
        return;
    };

    // 从 group.marks 移除（按指针相等）。
    let mut marks = group.marks.lock();
    let before = marks.len();
    marks.retain(|m| !Arc::ptr_eq(m, mark));
    let removed = before != marks.len();
    drop(marks);

    if removed {
        // 投递 IN_IGNORED：watch 被撤销（rm_watch/oneshot/DELETE_SELF/UNMOUNT 均经此路径）。
        // shutdown(fd close) 不调用 destroy_mark，故不误发。
        group.backend.notify_ignored(&group, mark);
        // 通知后端从其内部结构（wd 表）移除。
        group.backend.free_mark(mark);
        // 从全局索引移除。
        index_remove(mark);
        // 维护全局 watch 计数（唯一计数器，覆盖上限检查 + 快速路径）。
        adjust_total_watches(-1);
    }
}
