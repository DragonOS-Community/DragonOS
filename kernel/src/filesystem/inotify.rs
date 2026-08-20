//! inotify 文件系统事件通知设备层。
//!
//! 实现伪文件（`InotifyInode`：`IndexNode + PollableInode`）与 inotify 后端
//!（`InotifyBackend`：`FsNotifyBackend`），以及 4 个 syscall handler。
//!
//! 模式照搬 `eventfd.rs`：伪 FS + 伪 Inode + epoll 集成。
//!
//! 详见 `docs/kernel/filesystem/inotify.md` §4。

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::boxed::Box;
use hashbrown::HashMap;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::{SYS_INOTIFY_ADD_WATCH, SYS_INOTIFY_INIT1, SYS_INOTIFY_RM_WATCH};
// SYS_INOTIFY_INIT 仅存在于 x86_64（Linux generic syscall ABI 用 inotify_init1 替代）。
#[cfg(target_arch = "x86_64")]
use crate::arch::syscall::nr::SYS_INOTIFY_INIT;
use crate::arch::MMArch;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::{EPollEventType, EPollItem};
use crate::filesystem::fsnotify::{
    self, mark, EnqueueResult, FsEvent, FsNotifyBackend, FsNotifyDeleteState, FsNotifyGroup,
    FsNotifyMark,
};
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::file::{File, FileFlags, FileMode, FilePrivateData};
use crate::filesystem::vfs::permission::{check_inode_permission, PermissionMask};
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::{
    FileSystem, FileType, FsInfo, IndexNode, InodeMode, Magic, Metadata, PollableInode, SuperBlock,
    NAME_MAX, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::libs::casting::DowncastArc;
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::mm::MemoryManagementArch;
use crate::process::namespace::NamespaceOps;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::vfs_check_and_clone_cstr;

// ============================================================================
// 用户态 mask 位（与 Linux `include/uapi/linux/inotify.h` 完全一致）
// ============================================================================

/// `inotify_event.mask` 中的事件位与控制位。事件位与 [`FsEvent`] 低 16 位一致。
#[allow(dead_code)]
mod user_mask {
    pub const IN_ACCESS: u32 = 0x00000001;
    pub const IN_MODIFY: u32 = 0x00000002;
    pub const IN_ATTRIB: u32 = 0x00000004;
    pub const IN_CLOSE_WRITE: u32 = 0x00000008;
    pub const IN_CLOSE_NOWRITE: u32 = 0x00000010;
    pub const IN_OPEN: u32 = 0x00000020;
    pub const IN_MOVED_FROM: u32 = 0x00000040;
    pub const IN_MOVED_TO: u32 = 0x00000080;
    pub const IN_CREATE: u32 = 0x00000100;
    pub const IN_DELETE: u32 = 0x00000200;
    pub const IN_DELETE_SELF: u32 = 0x00000400;
    pub const IN_MOVE_SELF: u32 = 0x00000800;
    pub const IN_UNMOUNT: u32 = 0x00002000;
    pub const IN_Q_OVERFLOW: u32 = 0x00004000;
    pub const IN_IGNORED: u32 = 0x00008000;
    pub const IN_ONLYDIR: u32 = 0x01000000;
    pub const IN_DONT_FOLLOW: u32 = 0x02000000;
    pub const IN_EXCL_UNLINK: u32 = 0x04000000;
    pub const IN_MASK_CREATE: u32 = 0x10000000;
    pub const IN_MASK_ADD: u32 = 0x20000000;
    pub const IN_ISDIR: u32 = 0x40000000;
    pub const IN_ONESHOT: u32 = 0x80000000;

    /// add_watch 传入的合法控制位集合（用于 `from_bits` 校验）。
    pub const WATCH_CONTROL: u32 =
        IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK | IN_MASK_CREATE | IN_MASK_ADD | IN_ONESHOT;
    pub const ALL_INOTIFY_BITS: u32 =
        WATCH_CONTROL | 0x0000_0fff | IN_UNMOUNT | IN_Q_OVERFLOW | IN_IGNORED | IN_ISDIR;
}

bitflags::bitflags! {
    /// `inotify_init1` 的 flags（与 O_CLOEXEC/O_NONBLOCK 取值一致）。
    pub struct InotifyInitFlags: u32 {
        const IN_CLOEXEC = FileFlags::O_CLOEXEC.bits();
        const IN_NONBLOCK = FileFlags::O_NONBLOCK.bits();
    }
}

// ============================================================================
// 资源限制（常量，先不接 procfs sysctl；见设计文档 §6.1）
// ============================================================================

const MAX_USER_INSTANCES: usize = 128;
const MAX_USER_WATCHES: usize = 8192;
const MAX_QUEUED_EVENTS: usize = 16384;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct InotifyQuotaKey {
    user_namespace: usize,
    euid: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct InotifyQuotaCounts {
    instances: usize,
    watches: usize,
}

lazy_static::lazy_static! {
    static ref INOTIFY_QUOTAS: Mutex<HashMap<InotifyQuotaKey, InotifyQuotaCounts>> =
        Mutex::new(HashMap::new());
}

fn current_quota_key() -> InotifyQuotaKey {
    let cred = ProcessManager::current_pcb().cred();
    InotifyQuotaKey {
        user_namespace: cred.user_ns.ns_common().nsid.data(),
        euid: cred.euid.data(),
    }
}

fn reserve_instance(key: InotifyQuotaKey) -> Result<(), SystemError> {
    let mut quotas = INOTIFY_QUOTAS.lock();
    quotas.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
    let counts = quotas.entry(key).or_default();
    if counts.instances >= MAX_USER_INSTANCES {
        return Err(SystemError::EMFILE);
    }
    counts.instances += 1;
    Ok(())
}

fn reserve_watch(key: InotifyQuotaKey) -> Result<(), SystemError> {
    let mut quotas = INOTIFY_QUOTAS.lock();
    quotas.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
    let counts = quotas.entry(key).or_default();
    if counts.watches >= MAX_USER_WATCHES {
        return Err(SystemError::ENOSPC);
    }
    counts.watches += 1;
    Ok(())
}

fn release_quota(key: InotifyQuotaKey, instances: usize, watches: usize) {
    let mut quotas = INOTIFY_QUOTAS.lock();
    if let Some(counts) = quotas.get_mut(&key) {
        counts.instances = counts.instances.saturating_sub(instances);
        counts.watches = counts.watches.saturating_sub(watches);
        if counts.instances == 0 && counts.watches == 0 {
            quotas.remove(&key);
        }
    }
}

// ============================================================================
// 后端数据结构
// ============================================================================

/// 队列里的一个事件（已格式化为 inotify 语义，含 wd）。
#[derive(Debug)]
struct InotifyEventInfo {
    wd: i32,
    /// 已转为用户态 `IN_*` mask（含 ISDIR）。
    mask: u32,
    cookie: u32,
    /// 子项名（目录 watch 的子事件才有）。
    name: Option<String>,
}

/// 事件队列。受 `events` 锁保护。
#[derive(Debug)]
struct InotifyQueue {
    list: VecDeque<InotifyEventInfo>,
    /// A logical overflow record.  It cannot be stored in `list`: queue
    /// overflow and allocator failure are exactly the cases where growing the
    /// deque is not reliable.  `pre_overflow_remaining` preserves its position
    /// relative to ordinary records accepted before and after the loss.
    overflow_pending: bool,
    pre_overflow_remaining: usize,
}

/// watch descriptor 表。受 `wd` 锁保护。
#[derive(Debug)]
struct WdTable {
    /// 单调分配 wd（1..=i32::MAX-1，饱和见 §6.2；-1 被 Q_OVERFLOW 占用）。
    counter: i32,
    map: HashMap<i32, Weak<FsNotifyMark>>,
}

/// inotify 后端共享状态：同时被 `InotifyBackend`（在 group 内）与 `InotifyInode`（read 入口）持有。
///
/// 事件锁与 wd 锁分离，使 read（消费）与 add_watch/rm_watch（wd 管理）互不阻塞。
#[derive(Debug)]
pub struct InotifyState {
    /// 事件锁：所有 hook 都来自可睡眠的 VFS/file 操作上下文。
    events: Mutex<InotifyQueue>,
    /// One read must consume a contiguous queue prefix even though events are
    /// serialized outside `events`.
    read_consumer: Mutex<()>,
    max_queued_events: usize,
    /// wd 锁：add_watch/rm_watch 竞争。
    wd: Mutex<WdTable>,
    quota_key: InotifyQuotaKey,
}

impl InotifyState {
    fn new(quota_key: InotifyQuotaKey) -> Self {
        Self {
            events: Mutex::new(InotifyQueue {
                list: VecDeque::new(),
                overflow_pending: false,
                pre_overflow_remaining: 0,
            }),
            read_consumer: Mutex::new(()),
            max_queued_events: MAX_QUEUED_EVENTS,
            wd: Mutex::new(WdTable {
                counter: 0,
                map: HashMap::new(),
            }),
            quota_key,
        }
    }
}

/// inotify 后端（实现 [`FsNotifyBackend`]）。
#[derive(Debug)]
struct InotifyBackend {
    state: Arc<InotifyState>,
}

impl InotifyBackend {
    /// 入队一个事件（调用方已持 events 锁）。
    fn enqueue_locked(q: &mut InotifyQueue, max: usize, ev: InotifyEventInfo) -> EnqueueResult {
        // Linux compares wd/mask/name, but deliberately not the move cookie.
        // IN_IGNORED is never merged.  Do not merge across a pending logical
        // overflow boundary.
        if ev.mask != user_mask::IN_IGNORED
            && (!q.overflow_pending || q.pre_overflow_remaining < q.list.len())
            && q.list.back().is_some_and(|tail| {
                tail.wd == ev.wd && tail.mask == ev.mask && tail.name == ev.name
            })
        {
            return EnqueueResult::Merged;
        }

        if q.list.len().saturating_add(usize::from(q.overflow_pending)) >= max {
            if !q.overflow_pending {
                q.overflow_pending = true;
                q.pre_overflow_remaining = q.list.len();
                return EnqueueResult::DroppedQueueFull;
            }
            return EnqueueResult::DroppedQueueFull;
        }
        if q.list.try_reserve(1).is_err() {
            Self::record_overflow(q);
            return EnqueueResult::AllocationFailed;
        }
        q.list.push_back(ev);
        EnqueueResult::Queued
    }

    fn record_overflow(q: &mut InotifyQueue) -> bool {
        if q.overflow_pending {
            return false;
        }
        q.overflow_pending = true;
        q.pre_overflow_remaining = q.list.len();
        true
    }
}

impl FsNotifyBackend for InotifyBackend {
    fn handle_event(
        &self,
        group: &FsNotifyGroup,
        mark: &FsNotifyMark,
        mask: FsEvent,
        name: Option<&str>,
        cookie: u32,
    ) -> EnqueueResult {
        // 用户订阅 mask（ISDIR 始终保留，由 dispatch 设置）。
        let subscribed = mark.mask.load(Ordering::Relaxed);
        let user_mask = mask.bits() & (subscribed | FsEvent::ISDIR.bits());
        if user_mask == 0 {
            return EnqueueResult::Filtered;
        }

        // Truncate without allocating. Most events merge or hit a full queue;
        // decide those cases using the borrowed name before making a copy.
        let name = name.map(|n| {
            let mut end = core::cmp::min(n.len(), NAME_MAX);
            while !n.is_char_boundary(end) {
                end -= 1;
            }
            &n[..end]
        });
        let early = {
            let mut queue = self.state.events.lock();
            let readable_before = !queue.list.is_empty() || queue.overflow_pending;
            let merge = user_mask != user_mask::IN_IGNORED
                && (!queue.overflow_pending || queue.pre_overflow_remaining < queue.list.len())
                && queue.list.back().is_some_and(|tail| {
                    tail.wd == mark.wd && tail.mask == user_mask && tail.name.as_deref() == name
                });
            let result = if merge {
                Some(EnqueueResult::Merged)
            } else if queue
                .list
                .len()
                .saturating_add(usize::from(queue.overflow_pending))
                >= self.state.max_queued_events
            {
                Self::record_overflow(&mut queue);
                Some(EnqueueResult::DroppedQueueFull)
            } else {
                None
            };
            let wake = !readable_before && (!queue.list.is_empty() || queue.overflow_pending);
            (result, wake)
        };
        if early.1 {
            group.wait_queue.wakeup_all(None);
            let _ = EventPoll::wakeup_epoll(
                &group.epitems,
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        if let Some(result) = early.0 {
            return result;
        }

        let name = match name {
            Some(n) => {
                let mut owned = String::new();
                if owned.try_reserve_exact(n.len()).is_err() {
                    let wake = Self::record_overflow(&mut self.state.events.lock());
                    if wake {
                        group.wait_queue.wakeup_all(None);
                        let _ = EventPoll::wakeup_epoll(
                            &group.epitems,
                            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
                        );
                    }
                    return EnqueueResult::AllocationFailed;
                }
                owned.push_str(n);
                Some(owned)
            }
            None => None,
        };

        let ev = InotifyEventInfo {
            wd: mark.wd,
            mask: user_mask,
            cookie,
            name,
        };

        let (result, wake) = {
            let mut queue = self.state.events.lock();
            let readable_before = !queue.list.is_empty() || queue.overflow_pending;
            let result = Self::enqueue_locked(&mut queue, self.state.max_queued_events, ev);
            let readable_after = !queue.list.is_empty() || queue.overflow_pending;
            (result, !readable_before && readable_after)
        };

        // 唤醒 read 等待者与 epoll。
        if wake {
            group.wait_queue.wakeup_all(None);
            let _ = EventPoll::wakeup_epoll(
                &group.epitems,
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        result
    }

    fn free_mark(&self, mark: &FsNotifyMark) {
        let mut t = self.state.wd.lock();
        if t.map.remove(&mark.wd).is_some() {
            release_quota(self.state.quota_key, 0, 1);
        }
    }

    fn notify_ignored(&self, group: &FsNotifyGroup, mark: &FsNotifyMark) {
        // 投递 IN_IGNORED（watch 被撤销：rm_watch/oneshot/DELETE_SELF/UNMOUNT）。
        let wake = {
            let mut queue = self.state.events.lock();
            let readable_before = !queue.list.is_empty() || queue.overflow_pending;
            let _ = Self::enqueue_locked(
                &mut queue,
                self.state.max_queued_events,
                InotifyEventInfo {
                    wd: mark.wd,
                    mask: user_mask::IN_IGNORED,
                    cookie: 0,
                    name: None,
                },
            );
            !readable_before && (!queue.list.is_empty() || queue.overflow_pending)
        };
        if wake {
            group.wait_queue.wakeup_all(None);
            let _ = EventPoll::wakeup_epoll(
                &group.epitems,
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
    }

    fn queue_nonempty(&self) -> bool {
        let q = self.state.events.lock();
        !q.list.is_empty() || q.overflow_pending
    }
}

// ============================================================================
// 伪文件系统
// ============================================================================

lazy_static::lazy_static! {
    static ref INOTIFY_FS: Arc<InotifyFs> = Arc::new(InotifyFs);
}

/// inotify 伪文件系统（类比 `EventFdFs`，无真正挂载）。
#[derive(Debug)]
pub struct InotifyFs;

impl InotifyFs {
    pub fn instance() -> Arc<InotifyFs> {
        INOTIFY_FS.clone()
    }
}

impl FileSystem for InotifyFs {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // 不会被真正调用：inotify 不挂载。
        Arc::new(InotifyInode::new(
            false,
            InotifyQuotaKey {
                user_namespace: 0,
                euid: 0,
            },
        ))
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "inotify"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(Magic::INOTIFY_MAGIC, MMArch::PAGE_SIZE as u64, 255)
    }
}

// ============================================================================
// 伪 inode
// ============================================================================

/// inotify fd 对应的伪 inode。read/poll/epoll 委托给 group 与后端状态。
#[derive(Debug)]
pub struct InotifyInode {
    group: Arc<FsNotifyGroup>,
    state: Arc<InotifyState>,
    /// `O_NONBLOCK`：read 空队列时立即返回 EAGAIN。
    /// 用 AtomicBool 以支持 fcntl(F_SETFL) 动态修改（由 File::set_flags 同步）。
    nonblock: AtomicBool,
}

impl InotifyInode {
    fn validate_watch_mask(mask: u32) -> Result<(), SystemError> {
        if mask == 0 || (mask & !user_mask::ALL_INOTIFY_BITS) != 0 {
            Err(SystemError::EINVAL)
        } else {
            Ok(())
        }
    }

    /// 创建一个新的 inotify 实例（inotify_init1 用）。
    fn new(nonblock: bool, quota_key: InotifyQuotaKey) -> Self {
        let state = Arc::new(InotifyState::new(quota_key));
        let backend = Box::new(InotifyBackend {
            state: state.clone(),
        });
        let group = FsNotifyGroup::new(backend);
        Self {
            group,
            state,
            nonblock: AtomicBool::new(nonblock),
        }
    }

    /// 同步 O_NONBLOCK 状态（由 File::set_flags 在 fcntl F_SETFL 时调用）。
    pub fn set_nonblocking(&self, nb: bool) {
        self.nonblock.store(nb, Ordering::Relaxed);
    }

    /// name 域长度（含末尾 NUL，向上对齐到 sizeof(inotify_event)=16 字节）。
    /// Linux `roundup(name_len+1, sizeof(struct inotify_event))`。
    /// ABI 硬约束：对齐到 8 会导致多事件缓冲区错位。
    fn name_field_len(name_len: usize) -> usize {
        (name_len + 1 + 15) & !15
    }

    /// 计算一个事件序列化后的字节长度（固定头 16 + name 域）。
    fn record_len(name: Option<&str>) -> usize {
        const HEADER: usize = 16;
        match name {
            None => HEADER,
            Some(n) => HEADER + Self::name_field_len(n.len()),
        }
    }

    /// 序列化单个事件到 `out`（长度已由 `record_len` 保证足够）。返回写入字节数。
    fn serialize(ev: &InotifyEventInfo, out: &mut [u8]) -> usize {
        let name = ev.name.as_deref();
        let name_field = match name {
            None => 0usize,
            Some(n) => Self::name_field_len(n.len()),
        };
        out[0..4].copy_from_slice(&ev.wd.to_ne_bytes());
        out[4..8].copy_from_slice(&ev.mask.to_ne_bytes());
        out[8..12].copy_from_slice(&ev.cookie.to_ne_bytes());
        out[12..16].copy_from_slice(&(name_field as u32).to_ne_bytes());
        if let Some(n) = name {
            let nb = n.as_bytes();
            out[16..16 + nb.len()].copy_from_slice(nb);
            // NUL + 对齐填充。
            for b in &mut out[16 + nb.len()..16 + name_field] {
                *b = 0;
            }
        }
        16 + name_field
    }

    /// `inotify_add_watch`：在 `inode` 上建立（或更新）一个 watch。
    pub fn add_watch(&self, inode: Arc<dyn IndexNode>, mask: u32) -> Result<i32, SystemError> {
        if let Some(mounted) = inode
            .clone()
            .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
        {
            return mounted.with_fsnotify_admission(|| {
                mounted.with_fsnotify_watch_lifecycle(|lifecycle| {
                    self.add_watch_inner(inode, mask, Some(lifecycle))
                })
            });
        }
        self.add_watch_inner(inode, mask, None)
    }

    fn add_watch_inner(
        &self,
        inode: Arc<dyn IndexNode>,
        mask: u32,
        delete_lifecycle: Option<Arc<Mutex<FsNotifyDeleteState>>>,
    ) -> Result<i32, SystemError> {
        // mask 校验：仅允许已知位。
        Self::validate_watch_mask(mask)?;
        // IN_MASK_ADD 与 IN_MASK_CREATE 互斥。
        if (mask & user_mask::IN_MASK_ADD) != 0 && (mask & user_mask::IN_MASK_CREATE) != 0 {
            return Err(SystemError::EINVAL);
        }

        let md = inode.metadata()?;
        let target_identity = fsnotify::target_for_inode(&inode)?.id;

        // IN_ONLYDIR：目标必须是目录。
        if (mask & user_mask::IN_ONLYDIR) != 0 && md.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 读权限检查（防止通过监听泄露文件名/元数据）。
        check_inode_permission(&inode, &md, PermissionMask::MAY_READ)?;

        // Linux stores only user event bits and adds UNMOUNT implicitly;
        // ONESHOT/EXCL_UNLINK are mark flags, while ISDIR/Q_OVERFLOW/IGNORED
        // are output-only bits even though the syscall accepts them.
        let event_mask = (mask & 0x0000_0fff) | user_mask::IN_UNMOUNT;

        // 查找同 inode 上已有 mark（同 group）；若无，则在**同一把 marks 锁**内完成
        // 新建与插入，避免并发 add_watch 同一 inode 产生重复 mark（TOCTOU：查重与插入
        // 不在同一锁内时，两个线程可各自通过「无 existing」检查并各建一个 mark，导致
        // 重复事件且 rm_watch 无法彻底移除）。
        // 锁序 marks → wd → FSNOTIFY 为此处引入的嵌套；全代码库无反向获取
        // （destroy_mark/dispatch 均先释放 marks 再取 wd/FSNOTIFY），故无死锁。
        let mut marks = self.group.marks.lock();
        if let Some(existing) = marks.get(&target_identity) {
            if (mask & user_mask::IN_MASK_CREATE) != 0 {
                return Err(SystemError::EEXIST);
            }
            let _dispatch = existing.dispatch_lock.lock();
            if !existing.active.load(Ordering::Acquire) {
                let stale = existing.clone();
                drop(_dispatch);
                drop(marks);
                mark::destroy_mark(&stale);
                return self.add_watch_inner(inode, mask, delete_lifecycle);
            }
            if (mask & user_mask::IN_MASK_ADD) != 0 {
                existing.mask.fetch_or(event_mask, Ordering::Relaxed);
                // OR 语义：任一来源设置 oneshot 即生效（新 mask 或已有状态）。
                if (mask & user_mask::IN_ONESHOT) != 0 {
                    existing.oneshot.store(true, Ordering::Relaxed);
                }
                if (mask & user_mask::IN_EXCL_UNLINK) != 0 {
                    existing.excl_unlink.store(true, Ordering::Relaxed);
                }
            } else {
                existing.mask.store(event_mask, Ordering::Relaxed);
                existing
                    .oneshot
                    .store((mask & user_mask::IN_ONESHOT) != 0, Ordering::Relaxed);
                existing
                    .excl_unlink
                    .store((mask & user_mask::IN_EXCL_UNLINK) != 0, Ordering::Relaxed);
            }
            return Ok(existing.wd);
        }

        // Reserve every fallible container allocation before publication.
        marks.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;

        // 分配 wd（饱和到 i32::MAX-1；-1 被 Q_OVERFLOW 占用）。
        let wd = {
            let mut t = self.state.wd.lock();
            if t.counter >= i32::MAX - 1 {
                return Err(SystemError::ENOSPC);
            }
            t.map.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            t.counter += 1;
            t.counter
        };

        let mark = Arc::try_new(FsNotifyMark {
            wd,
            group: Arc::downgrade(&self.group),
            _inode: fsnotify::canonical_inode(inode.clone()),
            _delete_lifecycle: delete_lifecycle,
            object_id: target_identity,
            dispatch_lock: Mutex::new(()),
            active: AtomicBool::new(true),
            mask: AtomicU32::new(event_mask),
            oneshot: AtomicBool::new((mask & user_mask::IN_ONESHOT) != 0),
            excl_unlink: AtomicBool::new((mask & user_mask::IN_EXCL_UNLINK) != 0),
        })
        .map_err(|_| SystemError::ENOMEM)?;

        // Quota is committed only after all local allocations succeeded. Any
        // failure at the final global-index publication is rolled back below.
        reserve_watch(self.state.quota_key)?;
        fsnotify::adjust_total_watches(1);

        // 持 marks 锁完成全部插入（wd 表 / group.marks / 全局索引）。
        self.state.wd.lock().map.insert(wd, Arc::downgrade(&mark));
        marks.insert(target_identity, mark.clone());
        // The global index insertion is the publication point. Keep the group
        // management lock held until every structure dispatch relies on is
        // complete, so there is no visible-but-half-initialized window.
        if let Err(error) = fsnotify::index_add(&mark) {
            marks.remove(&target_identity);
            self.state.wd.lock().map.remove(&wd);
            fsnotify::adjust_total_watches(-1);
            release_quota(self.state.quota_key, 0, 1);
            return Err(error);
        }
        drop(marks);
        // 注：watch 计数已由 try_reserve_watch 原子 +1（含上限检查），此处不可再次 +1，
        // 否则每次 add 会 +2 而 destroy 仅 -1，导致 TOTAL_WATCHES 永不归零（fast-path 短路
        // 失效，read/write/close 热路径永久付出 fsnotify 锁开销）且上限提前触顶。

        Ok(wd)
    }

    /// `inotify_rm_watch`：按 wd 移除一个 watch。
    pub fn rm_watch(&self, wd: i32) -> Result<(), SystemError> {
        let mark = {
            let t = self.state.wd.lock();
            match t.map.get(&wd) {
                Some(w) => w.upgrade().ok_or(SystemError::EINVAL),
                None => Err(SystemError::EINVAL),
            }
        }?;
        // destroy_mark 完成 group.marks / 全局索引 / free_mark / 计数 收尾。
        mark::destroy_mark(&mark);
        Ok(())
    }

    /// fd 关闭收尾：撤销该实例所有 watch，回退计数。
    fn shutdown(&self) {
        let marks = {
            let mut g = self.group.marks.lock();
            core::mem::take(&mut *g)
        };
        let n = marks.len();
        for m in marks.values() {
            let dispatch = m.dispatch_lock.lock();
            m.active.store(false, Ordering::Release);
            drop(dispatch);
            fsnotify::index_remove(m);
            self.state.wd.lock().map.remove(&m.wd);
        }
        if n > 0 {
            fsnotify::adjust_total_watches(-(n as i32));
            release_quota(self.state.quota_key, 0, n);
        }
        release_quota(self.state.quota_key, 1, 0);
        // 唤醒任何阻塞在 read 的线程（队列不再增长）。
        self.group.wait_queue.wakeup_all(None);
    }
}

impl PollableInode for InotifyInode {
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        if self.group.backend.queue_nonempty() {
            Ok((EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize)
        } else {
            Ok(0)
        }
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.group.epitems.add(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.group.epitems.remove(epitem)
    }
}

impl IndexNode for InotifyInode {
    /// inotify fd 不可 seek：pread/pwrite/lseek → ESPIPE。
    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // fd 关闭：撤销该实例所有 watch（不发 IN_IGNORED——fd 已无消费者），
        // 回退实例/全局 watch 计数，唤醒任何阻塞的 reader。
        self.shutdown();
        Ok(())
    }

    /// read 语义见设计文档 §4.3。
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        // 1. buffer 小于事件头 → EINVAL。
        if len < 16 {
            return Err(SystemError::EINVAL);
        }

        let _consumer = self.state.read_consumer.lock();

        loop {
            let mut written = 0;
            loop {
                let mut blocked_by_size = false;
                let next = {
                    let mut q = self.state.events.lock();
                    if q.overflow_pending && q.pre_overflow_remaining == 0 {
                        if written + 16 > len {
                            blocked_by_size = true;
                            None
                        } else {
                            q.overflow_pending = false;
                            q.pre_overflow_remaining = 0;
                            Some(InotifyEventInfo {
                                wd: -1,
                                mask: user_mask::IN_Q_OVERFLOW,
                                cookie: 0,
                                name: None,
                            })
                        }
                    } else if let Some(front) = q.list.front() {
                        let record_len = Self::record_len(front.name.as_deref());
                        if written + record_len > len {
                            blocked_by_size = true;
                            None
                        } else {
                            let ev = q.list.pop_front().expect("front existed under events lock");
                            if q.overflow_pending && q.pre_overflow_remaining > 0 {
                                q.pre_overflow_remaining -= 1;
                            }
                            Some(ev)
                        }
                    } else {
                        None
                    }
                };

                let Some(ev) = next else {
                    let _ = blocked_by_size;
                    break;
                };
                let rl = Self::record_len(ev.name.as_deref());
                written += Self::serialize(&ev, &mut buf[written..written + rl]);
            }

            if written == 0 {
                let empty = {
                    let q = self.state.events.lock();
                    q.list.is_empty() && !q.overflow_pending
                };
                if empty {
                    // 空队列
                    if self.nonblock.load(Ordering::Relaxed) {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    if ProcessManager::current_pcb().has_pending_signal_fast() {
                        return Err(SystemError::ERESTARTSYS);
                    }
                    wq_wait_event_interruptible!(
                        self.group.wait_queue,
                        self.group.backend.queue_nonempty(),
                        {}
                    )?;
                    continue;
                }
                // 首个事件即放不下（name 过大）。
                return Err(SystemError::EINVAL);
            }
            return Ok(written);
        }
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EBADF)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(Metadata {
            mode: InodeMode::from_bits_truncate(0o400),
            file_type: FileType::File,
            ..Default::default()
        })
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        InotifyFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("inotify"))
    }
}

// ============================================================================
// init 实现
// ============================================================================

/// `inotify_init1` 的内核实现。
pub fn do_inotify_init1(flags: u32) -> Result<usize, SystemError> {
    let flags = InotifyInitFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;

    let quota_key = current_quota_key();
    reserve_instance(quota_key)?;

    let nonblock = flags.contains(InotifyInitFlags::IN_NONBLOCK);
    let inode = Arc::new(InotifyInode::new(nonblock, quota_key));
    let file_flags = FileFlags::O_RDONLY
        | (if nonblock {
            FileFlags::O_NONBLOCK
        } else {
            FileFlags::empty()
        });
    let file = File::new(inode, file_flags).inspect_err(|_| {
        // File::new 失败：file 未创建，不会 drop→shutdown，需手动回退实例计数。
        release_quota(quota_key, 1, 0);
    })?;
    // 防递归：inotify fd 自身的 read/write 不应产生事件。
    file.set_mode_flags(FileMode::FMODE_NONOTIFY);

    let cloexec = flags.contains(InotifyInitFlags::IN_CLOEXEC);
    let binding = ProcessManager::current_pcb().fd_table();
    let mut fd_table_guard = binding.write();
    // alloc_fd 失败时 file 被 drop → File::drop → close → shutdown → 回退实例计数，
    // 故此处不再手动回退。
    fd_table_guard
        .alloc_fd(file, None, cloexec)
        .map(|fd| fd as usize)
}

/// `inotify_init`（无参，等价 `inotify_init1(0)`）。
pub fn do_inotify_init() -> Result<usize, SystemError> {
    do_inotify_init1(0)
}

// ============================================================================
// syscall handlers
// ============================================================================

pub struct SysInotifyInitHandle;
impl Syscall for SysInotifyInitHandle {
    fn num_args(&self) -> usize {
        0
    }
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_inotify_init()
    }
    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}
#[cfg(target_arch = "x86_64")]
syscall_table_macros::declare_syscall!(SYS_INOTIFY_INIT, SysInotifyInitHandle);

pub struct SysInotifyInit1Handle;
impl SysInotifyInit1Handle {
    fn flags(args: &[usize]) -> u32 {
        args[0] as u32
    }
}
impl Syscall for SysInotifyInit1Handle {
    fn num_args(&self) -> usize {
        1
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_inotify_init1(Self::flags(args))
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", Self::flags(args)),
        )]
    }
}
syscall_table_macros::declare_syscall!(SYS_INOTIFY_INIT1, SysInotifyInit1Handle);

pub struct SysInotifyAddWatchHandle;
impl SysInotifyAddWatchHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn pathname(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    fn mask(args: &[usize]) -> u32 {
        args[2] as u32
    }
}
impl Syscall for SysInotifyAddWatchHandle {
    fn num_args(&self) -> usize {
        3
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let path_ptr = Self::pathname(args);
        let mask = Self::mask(args);

        // Linux validates unknown/zero masks before fdget. Pathname copying is
        // deliberately later, after fd/type/control validation.
        InotifyInode::validate_watch_mask(mask)?;

        // 取 inotify fd 对应的 InotifyInode（file Arc 保活，ino 借用其 inode）。
        let file: Arc<File> = {
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.read();
            fd_table_guard
                .get_file_by_fd(fd)
                .ok_or(SystemError::EBADF)?
        };
        let inode = file.inode();
        // IN_MASK_ADD and IN_MASK_CREATE are checked after fdget but before
        // verifying the descriptor type, matching Linux error priority.
        if (mask & user_mask::IN_MASK_ADD) != 0 && (mask & user_mask::IN_MASK_CREATE) != 0 {
            return Err(SystemError::EINVAL);
        }
        let inotify_inode = inode
            .as_any_ref()
            .downcast_ref::<InotifyInode>()
            .ok_or(SystemError::EINVAL)?;
        let path = vfs_check_and_clone_cstr(path_ptr, Some(crate::filesystem::vfs::MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        // 解析路径（IN_DONT_FOLLOW：不跟随末尾 symlink）。
        let pcb = ProcessManager::current_pcb();
        let (inode_begin, remain_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
        let target = if (mask & user_mask::IN_DONT_FOLLOW) != 0 {
            inode_begin.lookup_follow_symlink2(&remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES, false)?
        } else {
            inode_begin.lookup_follow_symlink(&remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?
        };

        inotify_inode.add_watch(target, mask).map(|wd| wd as usize)
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("mask", format!("{:#x}", Self::mask(args))),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_INOTIFY_ADD_WATCH, SysInotifyAddWatchHandle);

pub struct SysInotifyRmWatchHandle;
impl SysInotifyRmWatchHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn wd(args: &[usize]) -> i32 {
        args[1] as i32
    }
}
impl Syscall for SysInotifyRmWatchHandle {
    fn num_args(&self) -> usize {
        2
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let wd = Self::wd(args);
        let file: Arc<File> = {
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.read();
            fd_table_guard
                .get_file_by_fd(fd)
                .ok_or(SystemError::EBADF)?
        };
        let inode = file.inode();
        let inotify_inode = inode
            .as_any_ref()
            .downcast_ref::<InotifyInode>()
            .ok_or(SystemError::EINVAL)?;
        inotify_inode.rm_watch(wd).map(|_| 0)
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("wd", Self::wd(args).to_string()),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_INOTIFY_RM_WATCH, SysInotifyRmWatchHandle);
