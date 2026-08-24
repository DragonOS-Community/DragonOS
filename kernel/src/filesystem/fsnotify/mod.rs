//! 文件系统事件通知统一层（fsnotify）。
//!
//! 本模块是 VFS 写路径 hook 与具体后端（当前仅 inotify）之间的解耦层。
//! VFS hook 只调用 [`fsnotify`]。挂载文件系统的 watch 使用对象局部快照，
//! 非挂载对象才使用全局后备索引，事件在取得不可变快照后分发。
//!
//! 设计原则（见 `docs/kernel/filesystem/inotify.md` §0/§3）：
//! - `fsnotify()` 尽力而为，绝不影响 syscall 返回值；
//! - `fsnotify()` 内部只取本层自旋锁与 group 队列锁，绝不回调 `IndexNode` 写方法；
//! - 锁序：对象快照锁只用于 clone/publish，释放后才进入 mark 与 group 队列，永不反向。

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

// 事件 mask：对应 Linux 内核 `FS_*` 事件，其比特位与用户态 `IN_*` 完全一致，
// 故可直接作为用户态 mask 使用（仅 `ISDIR` 由 dispatch 按需设置）。
//
// 参考：Linux `include/uapi/linux/inotify.h`、`include/linux/fsnotify_backend.h`。
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
        const UNMOUNT      = 0x00002000; // IN_UNMOUNT（文件系统卸载）
        const Q_OVERFLOW   = 0x00004000; // IN_Q_OVERFLOW（队列溢出）
        const IN_IGNORED   = 0x00008000; // watch 被撤销（inode 删除/卸载）
        const ISDIR        = 0x40000000; // 事件对象是目录（由 dispatch 设置）
    }
}

/// 后端接口（最小抽象）。
///
/// fsnotify 层通过此 trait 调用具体后端，保持 VFS → fsnotify → inotify 单向依赖。
pub trait FsNotifyBackend: Send + Sync + core::fmt::Debug {
    /// 处理一个事件：格式化、（可选）合并、入队，并唤醒等待者。
    fn handle_event(
        &self,
        group: &FsNotifyGroup,
        mark: &FsNotifyMark,
        mask: FsEvent,
        name: Option<&str>,
        cookie: u32,
    ) -> EnqueueResult;
    /// mark 销毁时从后端内部结构（如 wd 表）移除。
    fn free_mark(&self, mark: &FsNotifyMark);
    /// mark 被撤销时向消费者投递一个 IN_IGNORED 事件（rm_watch/oneshot/DELETE_SELF/UNMOUNT）。
    /// fd close(shutdown) 路径不调用此方法。
    fn notify_ignored(&self, group: &FsNotifyGroup, mark: &FsNotifyMark);
    /// poll 用：队列是否非空。
    fn queue_nonempty(&self) -> bool;
}

/// 全局 watch 计数：绝大多数时刻为 0。`fsnotify` 的第一道闸门——
/// 无 watch 时零锁开销（对齐 Linux `i_fsnotify_mask` 快速跳过）。
static TOTAL_WATCHES: AtomicUsize = AtomicUsize::new(0);

/// move 事件 cookie 分配器：每次 rename 取一个，FROM/TO 共享。
/// 0 表示「无 move」，故从 1 开始，回绕时跳过 0。
static NEXT_COOKIE: AtomicU32 = AtomicU32::new(1);

/// 取一个新的非零 move cookie。
pub fn next_cookie() -> u32 {
    loop {
        let c = NEXT_COOKIE.fetch_add(1, Ordering::Relaxed);
        if c != 0 {
            return c;
        }
    }
}
// 全局 mark 索引：用 `(InodeId, dev_id)` 复合键反查「挂在该 inode 上的所有 mark」。
//
// 必须用复合键：FUSE 多挂载会复用相同 inode 号（如 FUSE_ROOT_ID=1），纯 InodeId 键
// 会跨挂载误匹配，导致事件泄露 / 误判已有 watch。
// 存 `Weak<FsNotifyMark>`：group 拥有 mark（强引用），索引只做查找，不阻止回收。
// dispatch 时 `Weak::upgrade()` 失败的死引用会被惰性剔除。
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

/// 记录一次 watch 计数变更（add +1，撤销 -1）。
pub(crate) fn adjust_total_watches(delta: i32) {
    if delta >= 0 {
        TOTAL_WATCHES.fetch_add(delta as usize, Ordering::Release);
    } else {
        TOTAL_WATCHES.fetch_sub((-delta) as usize, Ordering::AcqRel);
    }
}

/// 系统中是否存在任意 inotify watch。供 VFS 热路径（open/read/write/close）做廉价短路：
/// 无 watch 时完全跳过 parent 解析与 fsnotify 调用（零开销）。
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
/// 把 mark 从全局索引移除（按指针相等匹配，rm_watch / 撤销时调用）。
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

/// 统一事件投递入口。在 VFS 操作**成功之后**调用，尽力而为，不影响调用方返回值。
///
/// - `parent`：对子项事件（CREATE/DELETE/MOVED_*），传 `(父目录 inode, 子项名)`；
/// - `child`：对自身事件（DELETE_SELF/MOVE_SELF/MODIFY/CLOSE/OPEN/ATTRIB），传目标 inode；
/// - 二者可同时非空（如 unlink：父目录得 `IN_DELETE`，子项得 `IN_DELETE_SELF`）。
///
/// # 安全性
/// 内部只读 `inode.metadata()`（inode 活着、metadata 只读不锁），不调用任何写方法，
/// 避免在调用方持有的 VFS/File 锁下重入。
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
    // ① 快速路径：系统无任何 watch → 直接返回（read/write/close 热路径零成本）。
    if TOTAL_WATCHES.load(Ordering::Relaxed) == 0 {
        return;
    }

    // ② 预取 inode 元数据（inode 活着、metadata 只读不锁，安全）。
    // 事件的「主体」是 child（被创建/删除/移动/修改的对象）；IN_ISDIR 由主体是否为
    // 目录决定，对 parent/child 两类 watch 一视同仁。若无 child（仅父目录自身事件
    // 的退化情况），ISDIR 不置位。
    // 用 (inode_id, dev_id) 复合键：FUSE 多挂载复用相同 inode 号，必须加 dev_id 区分。
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

    // ③ 收集候选 mark 快照：临界区仅做哈希查表（秒放），不做后端工作。
    // (mark 强引用, name, is_parent)
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
    // 注：死 Weak 在 lock 内 upgrade 失败时被跳过；惰性清理留给 index_remove。

    // 事件路由（Linux 模型）：
    // - 命名空间事件 CREATE/DELETE/MOVED_FROM/MOVED_TO：仅父目录 watch 收（带 name）；
    // - 自身事件 DELETE_SELF/MOVE_SELF：仅子项自身 watch 收；
    // - 内容类事件 ACCESS/MODIFY/ATTRIB/CLOSE_*/OPEN：父目录 watch（带 name）与子项自身 watch
    //   均收——使「监听目录」能收到子文件被读/写/开关/改属性的事件（inotify 头号用例）。
    // 一次 fsnotify 调用可同时通知父目录与子项（如 unlink：父得 IN_DELETE，子得 IN_DELETE_SELF）。
    let self_only = FsEvent::DELETE_SELF | FsEvent::MOVE_SELF;
    let parent_only = FsEvent::CREATE | FsEvent::DELETE | FsEvent::MOVED_FROM | FsEvent::MOVED_TO;
    // 内容类事件（IN_EXCL_UNLINK 抑制对象）。
    let content_type = FsEvent::MODIFY
        | FsEvent::ACCESS
        | FsEvent::CLOSE_WRITE
        | FsEvent::CLOSE_NOWRITE
        | FsEvent::OPEN;

    // ④ 锁外投递。
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
        // 父 mark 收除 self_only 外的全部；自身 mark 收除 parent_only 外的全部。
        let routed = if is_parent {
            mask & !self_only
        } else {
            mask & !parent_only
        };
        if routed.is_empty() {
            continue;
        }

        // inode 死亡事件（DELETE_SELF/UNMOUNT）：无论 watch 是否订阅都必须撤销 mark
        // 并投递 IN_IGNORED（由 destroy_mark 无条件入队），否则 watch 泄漏强引用。
        let inode_death =
            routed.contains(FsEvent::DELETE_SELF) || routed.contains(FsEvent::UNMOUNT);

        let subscribed = mark.mask.load(Ordering::Relaxed);
        let mask_matches = (subscribed & routed.bits()) != 0;

        // 非 inode-death 事件：未订阅或被 EXCL_UNLINK 抑制时跳过。
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

        // dispatch 设置 ISDIR（主体是目录时）。
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

        // 撤销：inode 死亡（无条件）或 oneshot（订阅匹配后触发一次即撤销）。
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
