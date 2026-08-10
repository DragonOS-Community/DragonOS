//! 文件系统事件通知统一层（fsnotify）。
//!
//! 本模块是 VFS 写路径 hook 与具体后端（当前仅 inotify）之间的解耦层。
//! VFS hook 只调用 [`fsnotify`]，由它查全局 mark 索引并把事件分发给匹配的 watch。
//!
//! 设计原则（见 `docs/kernel/filesystem/inotify.md` §0/§3）：
//! - `fsnotify()` 尽力而为，绝不影响 syscall 返回值；
//! - `fsnotify()` 内部只取本层自旋锁与 group 队列锁，绝不回调 `IndexNode` 写方法；
//! - 锁序：`MountFSInode/File 锁` → `FSNOTIFY 全局锁` → `group 队列锁`，永不反向。

pub mod group;
pub mod mark;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use hashbrown::HashMap;

use crate::filesystem::vfs::{FileType, IndexNode, InodeId};
use crate::libs::spinlock::SpinLock;
use system_error::SystemError;

pub use group::FsNotifyGroup;
pub use mark::FsNotifyMark;

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
    );
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
type MarkIndex = HashMap<(InodeId, usize), Vec<alloc::sync::Weak<FsNotifyMark>>>;

lazy_static::lazy_static! {
    static ref FSNOTIFY_MARKS: SpinLock<MarkIndex> = SpinLock::new(HashMap::new());
}

/// 记录一次 watch 计数变更（add +1，撤销 -1）。仅用于短路，Relaxed 即可。
pub(crate) fn adjust_total_watches(delta: i32) {
    if delta >= 0 {
        TOTAL_WATCHES.fetch_add(delta as usize, Ordering::Relaxed);
    } else {
        TOTAL_WATCHES.fetch_sub((-delta) as usize, Ordering::Relaxed);
    }
}

/// 系统中是否存在任意 inotify watch。供 VFS 热路径（open/read/write/close）做廉价短路：
/// 无 watch 时完全跳过 parent 解析与 fsnotify 调用（零开销）。
pub fn has_any_watch() -> bool {
    TOTAL_WATCHES.load(Ordering::Relaxed) != 0
}

/// 原子地预留一个 watch 槽位（用于上限检查）。
/// 成功时 TOTAL_WATCHES 已 +1；超限时回退并返回 ENOSPC。
/// 这是唯一的全局 watch 计数器，同时服务「快速路径短路」和「max_user_watches 上限检查」。
pub(crate) fn try_reserve_watch(max: usize) -> Result<(), SystemError> {
    let prev = TOTAL_WATCHES.fetch_add(1, Ordering::Relaxed);
    if prev >= max {
        TOTAL_WATCHES.fetch_sub(1, Ordering::Relaxed);
        return Err(SystemError::ENOSPC);
    }
    Ok(())
}

pub(crate) fn index_add(mark: &Arc<FsNotifyMark>) {
    let key = mark.identity();
    let mut idx = FSNOTIFY_MARKS.lock_irqsave();
    idx.entry(key).or_default().push(Arc::downgrade(mark));
}
/// 把 mark 从全局索引移除（按指针相等匹配，rm_watch / 撤销时调用）。
pub(crate) fn index_remove(mark: &FsNotifyMark) {
    let key = mark.identity();
    let self_ptr = mark as *const FsNotifyMark;
    let mut idx = FSNOTIFY_MARKS.lock_irqsave();
    if let Some(vec) = idx.get_mut(&key) {
        let mut i = 0;
        while i < vec.len() {
            // 剔除：指针相等（同一个 mark），或 Weak 已死。
            let drop_it = match vec[i].upgrade() {
                Some(arc) => core::ptr::eq(Arc::as_ptr(&arc), self_ptr),
                None => true,
            };
            if drop_it {
                vec.swap_remove(i);
            } else {
                i += 1;
            }
        }
        if vec.is_empty() {
            idx.remove(&key);
        }
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
    // ① 快速路径：系统无任何 watch → 直接返回（read/write/close 热路径零成本）。
    if TOTAL_WATCHES.load(Ordering::Relaxed) == 0 {
        return;
    }

    // ② 预取 inode 元数据（inode 活着、metadata 只读不锁，安全）。
    // 事件的「主体」是 child（被创建/删除/移动/修改的对象）；IN_ISDIR 由主体是否为
    // 目录决定，对 parent/child 两类 watch 一视同仁。若无 child（仅父目录自身事件
    // 的退化情况），ISDIR 不置位。
    // 用 (inode_id, dev_id) 复合键：FUSE 多挂载复用相同 inode 号，必须加 dev_id 区分。
    let (child_key, event_is_dir, child_unlinked) = match child {
        Some(c) => match c.metadata() {
            Ok(md) => (
                Some((md.inode_id, md.dev_id)),
                md.file_type == FileType::Dir,
                md.nlinks == 0,
            ),
            Err(_) => (None, false, false),
        },
        None => (None, false, false),
    };
    let parent_key = parent.and_then(|(p, _)| p.metadata().ok().map(|md| (md.inode_id, md.dev_id)));

    if child_key.is_none() && parent_key.is_none() {
        return;
    }

    // ③ 收集候选 mark 快照：临界区仅做哈希查表（秒放），不做后端工作。
    // (mark 强引用, name, is_parent)
    let mut snapshot: Vec<(Arc<FsNotifyMark>, Option<&str>, bool)> = Vec::new();
    {
        let idx = FSNOTIFY_MARKS.lock_irqsave();
        if let Some(pk) = parent_key {
            let name = parent.map(|(_, n)| n);
            if let Some(vec) = idx.get(&pk) {
                for w in vec.iter() {
                    if let Some(m) = w.upgrade() {
                        snapshot.push((m, name, true));
                    }
                }
            }
        }
        if let Some(ck) = child_key {
            if let Some(vec) = idx.get(&ck) {
                for w in vec.iter() {
                    if let Some(m) = w.upgrade() {
                        snapshot.push((m, None, false));
                    }
                }
            }
        }
    }
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
        | FsEvent::ATTRIB
        | FsEvent::CLOSE_WRITE
        | FsEvent::CLOSE_NOWRITE
        | FsEvent::OPEN;

    // ④ 锁外投递。
    for (mark, name, is_parent) in snapshot {
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
            // IN_EXCL_UNLINK：仅对子项的「内容类」事件、且子项已 unlink 时抑制。
            if is_parent && mark.excl_unlink && routed.intersects(content_type) && child_unlinked {
                continue;
            }
        }

        // dispatch 设置 ISDIR（主体是目录时）。
        let mut delivered = routed;
        if event_is_dir {
            delivered |= FsEvent::ISDIR;
        }

        if let Some(group) = mark.group.upgrade() {
            group
                .backend
                .handle_event(&group, &mark, delivered, name, cookie);
        }

        // 撤销：inode 死亡（无条件）或 oneshot（订阅匹配后触发一次即撤销）。
        if inode_death || mark.oneshot.load(Ordering::Relaxed) {
            mark::destroy_mark(&mark);
        }
    }
}
