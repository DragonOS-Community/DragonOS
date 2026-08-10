//! inotify 文件系统事件通知设备层。
//!
//! 实现伪文件（`InotifyInode`：`IndexNode + PollableInode`）与 inotify 后端
//!（`InotifyBackend`：`FsNotifyBackend`），以及 4 个 syscall handler。
//!
//! 模式照搬 `eventfd.rs`：伪 FS + 伪 Inode + epoll 集成。
//!
//! 详见 `docs/kernel/filesystem/inotify.md` §4。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use alloc::boxed::Box;
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
    self, mark, FsEvent, FsNotifyBackend, FsNotifyGroup, FsNotifyMark,
};
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::file::{File, FileFlags, FileMode, FilePrivateData};
use crate::filesystem::vfs::permission::{check_inode_permission, PermissionMask};
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::{
    FileSystem, FileType, FsInfo, IndexNode, InodeMode, Magic, Metadata, PollableInode, SuperBlock,
    NAME_MAX, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::libs::spinlock::SpinLock;
use crate::mm::MemoryManagementArch;
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

    /// 仅 add-time 控制位（更新/订阅时应从 mask 剥离）。
    pub const ADD_TIME_ONLY: u32 = IN_ONLYDIR | IN_DONT_FOLLOW | IN_MASK_CREATE | IN_MASK_ADD;
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

/// 全局 inotify 实例计数（近似 per-user；DragonOS 当前单用户语义）。
static TOTAL_INSTANCES: AtomicUsize = AtomicUsize::new(0);

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
    name: Option<Box<str>>,
}

/// 事件队列。受 `events` 锁保护。
#[derive(Debug)]
struct InotifyQueue {
    list: VecDeque<InotifyEventInfo>,
    /// 置位后已插入一个 `IN_Q_OVERFLOW(wd=-1)`；消费该事件时清零。
    overflowed: bool,
}

/// watch descriptor 表。受 `wd` 锁保护。
#[derive(Debug)]
struct WdTable {
    /// 单调分配 wd（1..=i32::MAX-1，饱和见 §6.2；-1 被 Q_OVERFLOW 占用）。
    counter: i32,
    map: BTreeMap<i32, Weak<FsNotifyMark>>,
}

/// inotify 后端共享状态：同时被 `InotifyBackend`（在 group 内）与 `InotifyInode`（read 入口）持有。
///
/// 事件锁与 wd 锁分离，使 read（消费）与 add_watch/rm_watch（wd 管理）互不阻塞。
#[derive(Debug)]
pub struct InotifyState {
    /// 事件锁：handle_event(生产) 与 read(消费) 竞争。irqsave：fsnotify 可在持 VFS 锁时调用。
    events: SpinLock<InotifyQueue>,
    max_queued_events: usize,
    /// wd 锁：add_watch/rm_watch 竞争。
    wd: Mutex<WdTable>,
}

impl InotifyState {
    fn new() -> Self {
        Self {
            events: SpinLock::new(InotifyQueue {
                list: VecDeque::new(),
                overflowed: false,
            }),
            max_queued_events: MAX_QUEUED_EVENTS,
            wd: Mutex::new(WdTable {
                counter: 0,
                map: BTreeMap::new(),
            }),
        }
    }
}

/// inotify 后端（实现 [`FsNotifyBackend`]）。
#[derive(Debug)]
struct InotifyBackend {
    state: Arc<InotifyState>,
}

impl InotifyBackend {
    /// 事件是否可合并（仅 ACCESS/MODIFY 且无 name，见 §6.3）。
    fn mergeable(mask: FsEvent) -> bool {
        let core = mask - FsEvent::ISDIR;
        core == FsEvent::ACCESS || core == FsEvent::MODIFY
    }

    /// 入队一个事件（调用方已持 events 锁）。
    fn enqueue_locked(q: &mut InotifyQueue, max: usize, ev: InotifyEventInfo) {
        if q.list.len() >= max {
            // 队列满：丢弃当前事件，置 overflow 标志并插入单个 Q_OVERFLOW（仅一次）。
            if !q.overflowed {
                q.overflowed = true;
                q.list.push_back(InotifyEventInfo {
                    wd: -1,
                    mask: user_mask::IN_Q_OVERFLOW,
                    cookie: 0,
                    name: None,
                });
            }
            return;
        }
        q.list.push_back(ev);
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
    ) {
        // 用户订阅 mask（ISDIR 始终保留，由 dispatch 设置）。
        let subscribed = mark.mask.load(Ordering::Relaxed);
        let user_mask = mask.bits() & (subscribed | FsEvent::ISDIR.bits());
        if user_mask == 0 {
            return;
        }

        // name 截断到 NAME_MAX。
        let name = name.map(|n| {
            if n.len() > NAME_MAX {
                &n[..NAME_MAX]
            } else {
                n
            }
        });

        let ev = InotifyEventInfo {
            wd: mark.wd,
            mask: user_mask,
            cookie,
            name: name.map(Box::<str>::from),
        };

        let wake = {
            let mut q = self.state.events.lock_irqsave();
            // 合并：末尾事件 (wd, mask, cookie, name) 完全相同且可合并类 → 丢弃。
            if Self::mergeable(mask) && ev.name.is_none() {
                if let Some(tail) = q.list.back() {
                    if tail.wd == ev.wd
                        && tail.mask == ev.mask
                        && tail.cookie == ev.cookie
                        && tail.name.is_none()
                    {
                        return;
                    }
                }
            }
            Self::enqueue_locked(&mut q, self.state.max_queued_events, ev);
            true
        };

        // 唤醒 read 等待者与 epoll。
        group.wait_queue.wakeup_all(None);
        let _ = EventPoll::wakeup_epoll(
            &group.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
        let _ = wake;
    }

    fn free_mark(&self, mark: &FsNotifyMark) {
        let mut t = self.state.wd.lock();
        t.map.remove(&mark.wd);
    }

    fn notify_ignored(&self, group: &FsNotifyGroup, mark: &FsNotifyMark) {
        // 投递 IN_IGNORED（watch 被撤销：rm_watch/oneshot/DELETE_SELF/UNMOUNT）。
        let wake = {
            let mut q = self.state.events.lock_irqsave();
            Self::enqueue_locked(
                &mut q,
                self.state.max_queued_events,
                InotifyEventInfo {
                    wd: mark.wd,
                    mask: user_mask::IN_IGNORED,
                    cookie: 0,
                    name: None,
                },
            );
            true
        };
        group.wait_queue.wakeup_all(None);
        let _ = EventPoll::wakeup_epoll(
            &group.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
        let _ = wake;
    }

    fn queue_nonempty(&self) -> bool {
        !self.state.events.lock_irqsave().list.is_empty()
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
        Arc::new(InotifyInode::new(false))
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
    /// 创建一个新的 inotify 实例（inotify_init1 用）。
    pub fn new(nonblock: bool) -> Self {
        let state = Arc::new(InotifyState::new());
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
        // mask 校验：仅允许已知位。
        if (mask
            & !(user_mask::WATCH_CONTROL
            | 0x00000fff // IN_ALL_EVENTS 低 12 位
            | user_mask::IN_UNMOUNT
            | user_mask::IN_Q_OVERFLOW
            | user_mask::IN_IGNORED))
            != 0
        {
            return Err(SystemError::EINVAL);
        }

        // mask 必须含至少一个事件位（低 12 位 IN_ALL_EVENTS）。
        if (mask & 0x0000_0fff) == 0 {
            return Err(SystemError::EINVAL);
        }
        // IN_MASK_ADD 与 IN_MASK_CREATE 互斥。
        if (mask & user_mask::IN_MASK_ADD) != 0 && (mask & user_mask::IN_MASK_CREATE) != 0 {
            return Err(SystemError::EINVAL);
        }

        let md = inode.metadata()?;
        let target_identity = (md.inode_id, md.dev_id);

        // IN_ONLYDIR：目标必须是目录。
        if (mask & user_mask::IN_ONLYDIR) != 0 && md.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 读权限检查（防止通过监听泄露文件名/元数据）。
        check_inode_permission(&inode, &md, PermissionMask::MAY_READ)?;

        // 订阅位：剥离 add-time 控制位，保留 ONESHOT/EXCL_UNLINK。
        let event_mask = mask & !user_mask::ADD_TIME_ONLY;

        // 查找同 inode 上已有 mark（同 group）；若无，则在**同一把 marks 锁**内完成
        // 新建与插入，避免并发 add_watch 同一 inode 产生重复 mark（TOCTOU：查重与插入
        // 不在同一锁内时，两个线程可各自通过「无 existing」检查并各建一个 mark，导致
        // 重复事件且 rm_watch 无法彻底移除）。
        // 锁序 marks → wd → FSNOTIFY 为此处引入的嵌套；全代码库无反向获取
        // （destroy_mark/dispatch 均先释放 marks 再取 wd/FSNOTIFY），故无死锁。
        let mut marks = self.group.marks.lock();
        if let Some(existing) = marks.iter().find(|m| m.identity() == target_identity) {
            if (mask & user_mask::IN_MASK_CREATE) != 0 {
                return Err(SystemError::EEXIST);
            }
            if (mask & user_mask::IN_MASK_ADD) != 0 {
                existing.mask.fetch_or(event_mask, Ordering::Relaxed);
                // OR 语义：任一来源设置 oneshot 即生效（新 mask 或已有状态）。
                if (mask & user_mask::IN_ONESHOT) != 0 {
                    existing.oneshot.store(true, Ordering::Relaxed);
                }
            } else {
                existing.mask.store(event_mask, Ordering::Relaxed);
                existing
                    .oneshot
                    .store((mask & user_mask::IN_ONESHOT) != 0, Ordering::Relaxed);
            }
            return Ok(existing.wd);
        }

        // watch 上限（唯一全局计数器：同时服务快速路径与上限检查）。
        fsnotify::try_reserve_watch(MAX_USER_WATCHES)?;

        // 分配 wd（饱和到 i32::MAX-1；-1 被 Q_OVERFLOW 占用）。
        let wd = {
            let mut t = self.state.wd.lock();
            if t.counter >= i32::MAX - 1 {
                fsnotify::adjust_total_watches(-1);
                return Err(SystemError::ENOSPC);
            }
            t.counter += 1;
            t.counter
        };

        let mark = Arc::new(FsNotifyMark {
            wd,
            group: Arc::downgrade(&self.group),
            inode: inode.clone(),
            mask: AtomicU32::new(event_mask),
            oneshot: AtomicBool::new((mask & user_mask::IN_ONESHOT) != 0),
            excl_unlink: (mask & user_mask::IN_EXCL_UNLINK) != 0,
        });

        // 持 marks 锁完成全部插入（wd 表 / group.marks / 全局索引）。
        self.state.wd.lock().map.insert(wd, Arc::downgrade(&mark));
        marks.push(mark.clone());
        drop(marks);
        fsnotify::index_add(&mark);
        // 注：watch 计数已由 try_reserve_watch 原子 +1（含上限检查），此处不可再次 +1，
        // 否则每次 add 会 +2 而 destroy 仅 -1，导致 TOTAL_WATCHES 永不归零（fast-path 短路
        // 失效，read/write/close 热路径永久付出 fsnotify 锁开销）且上限提前触顶。

        Ok(wd)
    }

    /// `inotify_rm_watch`：按 wd 移除一个 watch。
    pub fn rm_watch(&self, wd: i32) -> Result<(), SystemError> {
        let mark = {
            let mut t = self.state.wd.lock();
            match t.map.remove(&wd) {
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
        for m in &marks {
            fsnotify::index_remove(m);
            self.state.wd.lock().map.remove(&m.wd);
        }
        if n > 0 {
            fsnotify::adjust_total_watches(-(n as i32));
        }
        TOTAL_INSTANCES.fetch_sub(1, Ordering::Relaxed);
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

        loop {
            let mut written;
            {
                let mut q = self.state.events.lock_irqsave();
                if q.list.is_empty() {
                    // 空队列
                    drop(q);
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

                // 逐个从头部打包完整事件，放不下的留在队列下次读。
                written = 0;
                while let Some(front) = q.list.front() {
                    let rl = Self::record_len(front.name.as_deref());
                    if written + rl > len {
                        break;
                    }
                    let ev = q.list.pop_front().unwrap();
                    if ev.mask == user_mask::IN_Q_OVERFLOW {
                        q.overflowed = false;
                    }
                    written += Self::serialize(&ev, &mut buf[written..written + rl]);
                }
            }

            if written == 0 {
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

    // 实例上限。
    if TOTAL_INSTANCES.fetch_add(1, Ordering::Relaxed) >= MAX_USER_INSTANCES {
        TOTAL_INSTANCES.fetch_sub(1, Ordering::Relaxed);
        return Err(SystemError::EMFILE);
    }

    let nonblock = flags.contains(InotifyInitFlags::IN_NONBLOCK);
    let inode = Arc::new(InotifyInode::new(nonblock));
    let file_flags = FileFlags::O_RDONLY
        | (if nonblock {
            FileFlags::O_NONBLOCK
        } else {
            FileFlags::empty()
        });
    let file = File::new(inode, file_flags).inspect_err(|_| {
        // File::new 失败：file 未创建，不会 drop→shutdown，需手动回退实例计数。
        TOTAL_INSTANCES.fetch_sub(1, Ordering::Relaxed);
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

        let path = vfs_check_and_clone_cstr(path_ptr, Some(crate::filesystem::vfs::MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        // 取 inotify fd 对应的 InotifyInode（file Arc 保活，ino 借用其 inode）。
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
