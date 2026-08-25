//! inotify filesystem event notification device layer.
//!
//! Implements the pseudo-file (`InotifyInode`: `IndexNode + PollableInode`) and the inotify backend
//! (`InotifyBackend`: `FsNotifyBackend`), plus 4 syscall handlers.
//!
//! The pattern mirrors `eventfd.rs`: pseudo FS + pseudo inode + epoll integration.
//!
//! See `docs/kernel/filesystem/inotify.md` §4 for details.

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
// SYS_INOTIFY_INIT exists only on x86_64 (the Linux generic syscall ABI uses inotify_init1 instead).
#[cfg(target_arch = "x86_64")]
use crate::arch::syscall::nr::SYS_INOTIFY_INIT;
use crate::arch::MMArch;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::{EPollEventType, EPollItem};
use crate::filesystem::fsnotify::{
    self, mark, EnqueueResult, FsEvent, FsNotifyBackend, FsNotifyGroup, FsNotifyMark,
    FsNotifyObjectId, FsNotifyObjectState,
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
use crate::syscall::user_access::{vfs_check_and_clone_cstr, UserBufferWriter};
use crate::syscall::user_buffer::UserBuffer;

/// Linux `FIONREAD`: report the number of bytes readable without blocking.
const FIONREAD: u32 = 0x541B;

// ============================================================================
// Userspace mask bits (identical to Linux `include/uapi/linux/inotify.h`)
// ============================================================================

/// Event and control bits in `inotify_event.mask`. The event bits match the low 16 bits of [`FsEvent`].
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

    /// Set of valid control bits accepted by add_watch (used for `from_bits` validation).
    pub const WATCH_CONTROL: u32 =
        IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK | IN_MASK_CREATE | IN_MASK_ADD | IN_ONESHOT;
    pub const ALL_INOTIFY_BITS: u32 =
        WATCH_CONTROL | 0x0000_0fff | IN_UNMOUNT | IN_Q_OVERFLOW | IN_IGNORED | IN_ISDIR;
}

bitflags::bitflags! {
    /// Flags for `inotify_init1` (values match O_CLOEXEC/O_NONBLOCK).
    pub struct InotifyInitFlags: u32 {
        const IN_CLOEXEC = FileFlags::O_CLOEXEC.bits();
        const IN_NONBLOCK = FileFlags::O_NONBLOCK.bits();
    }
}

// ============================================================================
// Resource limits (constants, not yet wired up to procfs sysctl; see design doc §6.1)
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

fn current_quota_keys() -> Result<Vec<InotifyQuotaKey>, SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    let mut namespace = cred.user_ns.clone();
    let mut keys = Vec::new();
    keys.try_reserve(namespace.level() as usize + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    keys.push(InotifyQuotaKey {
        user_namespace: cred.user_ns.ns_common().nsid.data(),
        euid: cred.euid.data(),
    });

    // Linux ucounts charge the current (user namespace, euid) and every
    // ancestor account that owns a namespace on the path to init_user_ns.
    // This prevents creating nested user namespaces from resetting inotify
    // limits while preserving the creator identity used at each boundary.
    while let Some(parent) = namespace.parent_ns() {
        keys.push(InotifyQuotaKey {
            user_namespace: parent.ns_common().nsid.data(),
            euid: namespace.inner.lock().owner,
        });
        namespace = parent;
    }
    Ok(keys)
}

fn reserve_instance(keys: &[InotifyQuotaKey]) -> Result<(), SystemError> {
    let mut quotas = INOTIFY_QUOTAS.lock();
    quotas
        .try_reserve(keys.len())
        .map_err(|_| SystemError::ENOMEM)?;
    if keys.iter().any(|key| {
        quotas
            .get(key)
            .is_some_and(|counts| counts.instances >= MAX_USER_INSTANCES)
    }) {
        return Err(SystemError::EMFILE);
    }
    for key in keys {
        quotas.entry(*key).or_default().instances += 1;
    }
    Ok(())
}

fn reserve_watch(keys: &[InotifyQuotaKey]) -> Result<(), SystemError> {
    let mut quotas = INOTIFY_QUOTAS.lock();
    quotas
        .try_reserve(keys.len())
        .map_err(|_| SystemError::ENOMEM)?;
    if keys.iter().any(|key| {
        quotas
            .get(key)
            .is_some_and(|counts| counts.watches >= MAX_USER_WATCHES)
    }) {
        return Err(SystemError::ENOSPC);
    }
    for key in keys {
        quotas.entry(*key).or_default().watches += 1;
    }
    Ok(())
}

fn release_quota(keys: &[InotifyQuotaKey], instances: usize, watches: usize) {
    let mut quotas = INOTIFY_QUOTAS.lock();
    for key in keys {
        if let Some(counts) = quotas.get_mut(key) {
            counts.instances = counts.instances.saturating_sub(instances);
            counts.watches = counts.watches.saturating_sub(watches);
            if counts.instances == 0 && counts.watches == 0 {
                quotas.remove(key);
            }
        }
    }
}

// ============================================================================
// Backend data structures
// ============================================================================

/// One event in the queue (already formatted to inotify semantics, including wd).
#[derive(Debug)]
struct InotifyEventInfo {
    wd: i32,
    /// Already converted to the userspace `IN_*` mask (including ISDIR).
    mask: u32,
    cookie: u32,
    /// Child name (only present for child events of a directory watch).
    name: Option<String>,
}

/// Event queue. Protected by the `events` lock.
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

/// Watch descriptor table. Protected by the `wd` lock.
#[derive(Debug)]
struct WdTable {
    /// Monotonically allocates wd (1..=i32::MAX-1, saturating per §6.2; -1 is reserved for Q_OVERFLOW).
    counter: i32,
    map: HashMap<i32, Weak<FsNotifyMark>>,
}

/// inotify backend shared state: held by both `InotifyBackend` (inside the group) and `InotifyInode` (the read entry point).
///
/// The event lock and the wd lock are separate, so read (consume) and add_watch/rm_watch (wd management) do not block each other.
#[derive(Debug)]
pub struct InotifyState {
    /// Event lock: all hooks come from sleepable VFS/file operation contexts.
    events: Mutex<InotifyQueue>,
    /// One read must consume a contiguous queue prefix even though events are
    /// serialized outside `events`.
    max_queued_events: usize,
    /// wd lock: contended by add_watch/rm_watch.
    wd: Mutex<WdTable>,
    quota_keys: Vec<InotifyQuotaKey>,
}

impl InotifyState {
    fn new(quota_keys: Vec<InotifyQuotaKey>) -> Self {
        Self {
            events: Mutex::new(InotifyQueue {
                list: VecDeque::new(),
                overflow_pending: false,
                pre_overflow_remaining: 0,
            }),
            max_queued_events: MAX_QUEUED_EVENTS,
            wd: Mutex::new(WdTable {
                counter: 0,
                map: HashMap::new(),
            }),
            quota_keys,
        }
    }
}

/// inotify backend (implements [`FsNotifyBackend`]).
#[derive(Debug)]
struct InotifyBackend {
    state: Arc<InotifyState>,
}

impl InotifyBackend {
    /// Enqueue an event (the caller already holds the events lock).
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
        // Userspace subscribed mask (ISDIR is always retained, set by dispatch).
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

        // Wake up read waiters and epoll.
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
            release_quota(&self.state.quota_keys, 0, 1);
        }
    }

    fn notify_ignored(&self, group: &FsNotifyGroup, mark: &FsNotifyMark) {
        // Deliver IN_IGNORED (the watch was revoked: rm_watch/oneshot/DELETE_SELF/UNMOUNT).
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
// Pseudo filesystem
// ============================================================================

lazy_static::lazy_static! {
    static ref INOTIFY_FS: Arc<InotifyFs> = Arc::new(InotifyFs);
}

/// inotify pseudo filesystem (analogous to `EventFdFs`, not actually mounted).
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
        // Never actually called: inotify is not mounted.
        Arc::new(InotifyInode::new(
            false,
            vec![InotifyQuotaKey {
                user_namespace: 0,
                euid: 0,
            }],
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
// Pseudo inode
// ============================================================================

/// Pseudo inode backing an inotify fd. read/poll/epoll delegate to the group and backend state.
#[derive(Debug)]
pub struct InotifyInode {
    group: Arc<FsNotifyGroup>,
    state: Arc<InotifyState>,
    /// Anonymous inotify files cannot be reopened through `/proc/self/fd`.
    /// Duplicated descriptors share the original `File` and do not call
    /// `IndexNode::open` again.
    opened: AtomicBool,
    /// `O_NONBLOCK`: read returns EAGAIN immediately when the queue is empty.
    /// Backed by AtomicBool to support dynamic modification via fcntl(F_SETFL) (synchronized by File::set_flags).
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

    /// Create a new inotify instance (used by inotify_init1).
    fn new(nonblock: bool, quota_keys: Vec<InotifyQuotaKey>) -> Self {
        let state = Arc::new(InotifyState::new(quota_keys));
        let backend = Box::new(InotifyBackend {
            state: state.clone(),
        });
        let group = FsNotifyGroup::new(backend);
        Self {
            group,
            state,
            opened: AtomicBool::new(false),
            nonblock: AtomicBool::new(nonblock),
        }
    }

    /// Synchronize the O_NONBLOCK state (called by File::set_flags on fcntl F_SETFL).
    pub fn set_nonblocking(&self, nb: bool) {
        self.nonblock.store(nb, Ordering::Relaxed);
    }

    /// Length of the name field (including the trailing NUL, rounded up to sizeof(inotify_event)=16 bytes).
    /// Linux `roundup(name_len+1, sizeof(struct inotify_event))`。
    /// Hard ABI constraint: aligning to 8 would misalign a multi-event buffer.
    fn name_field_len(name_len: usize) -> usize {
        (name_len + 1 + 15) & !15
    }

    /// Compute the serialized byte length of one event (fixed 16-byte header + name field).
    fn record_len(name: Option<&str>) -> usize {
        const HEADER: usize = 16;
        match name {
            None => HEADER,
            Some(n) => HEADER + Self::name_field_len(n.len()),
        }
    }

    /// Return the serialized size of the current logical queue.
    ///
    /// The overflow notification is tracked separately from `list`, but it is
    /// still one readable, header-only `inotify_event` and must be included.
    fn queued_bytes(&self) -> usize {
        const HEADER: usize = 16;
        let queue = self.state.events.lock();
        queue
            .list
            .iter()
            .map(|event| Self::record_len(event.name.as_deref()))
            .sum::<usize>()
            + usize::from(queue.overflow_pending) * HEADER
    }

    /// Serialize one event into `out` (the length is already guaranteed sufficient by `record_len`). Returns the number of bytes written.
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
            // NUL + alignment padding.
            for b in &mut out[16 + nb.len()..16 + name_field] {
                *b = 0;
            }
        }
        16 + name_field
    }

    /// Consume one read syscall's contiguous queue prefix.
    ///
    /// `write_record` runs without the queue lock. Concurrent readers claim
    /// records one at a time under `events`, matching Linux's notification-lock
    /// boundary. An error consumes the dequeued record and is returned even
    /// after earlier progress, matching Linux's inotify EFAULT rule.
    fn read_events_with(
        &self,
        len: usize,
        mut write_record: impl FnMut(usize, &InotifyEventInfo) -> Result<(), SystemError>,
    ) -> Result<usize, SystemError> {
        if len < 16 {
            return Err(SystemError::EINVAL);
        }

        loop {
            let mut written = 0usize;
            let mut blocked_by_size = false;

            loop {
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
                    break;
                };
                write_record(written, &ev)?;
                written += Self::record_len(ev.name.as_deref());
            }

            if written != 0 {
                return Ok(written);
            }
            if blocked_by_size {
                return Err(SystemError::EINVAL);
            }

            let empty = {
                let q = self.state.events.lock();
                q.list.is_empty() && !q.overflow_pending
            };
            if !empty {
                continue;
            }

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
        }
    }

    /// `inotify_add_watch`: establish (or update) a watch on `inode`.
    pub fn add_watch(&self, inode: Arc<dyn IndexNode>, mask: u32) -> Result<i32, SystemError> {
        if let Some(mounted) = inode
            .clone()
            .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
        {
            return mounted.with_fsnotify_admission(|| {
                let (target_identity, event_mask) = self.preflight_watch(&inode, mask)?;
                mounted.with_fsnotify_watch_lifecycle(|lifecycle| {
                    self.add_watch_inner(inode, mask, event_mask, target_identity, Some(lifecycle))
                })
            });
        }
        let (target_identity, event_mask) = self.preflight_watch(&inode, mask)?;
        self.add_watch_inner(inode, mask, event_mask, target_identity, None)
    }

    fn preflight_watch(
        &self,
        inode: &Arc<dyn IndexNode>,
        mask: u32,
    ) -> Result<(FsNotifyObjectId, u32), SystemError> {
        // mask validation: only known bits are allowed.
        Self::validate_watch_mask(mask)?;
        // IN_MASK_ADD and IN_MASK_CREATE are mutually exclusive.
        if (mask & user_mask::IN_MASK_ADD) != 0 && (mask & user_mask::IN_MASK_CREATE) != 0 {
            return Err(SystemError::EINVAL);
        }

        let md = inode.metadata()?;
        let target_identity = fsnotify::target_for_inode(inode)?.id;

        // IN_ONLYDIR: the target must be a directory.
        if (mask & user_mask::IN_ONLYDIR) != 0 && md.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // Read permission check (prevents leaking file names/metadata via watching).
        check_inode_permission(inode, &md, PermissionMask::MAY_READ)?;

        // Linux stores only user event bits and adds UNMOUNT implicitly;
        // ONESHOT/EXCL_UNLINK are mark flags, while ISDIR/Q_OVERFLOW/IGNORED
        // are output-only bits even though the syscall accepts them.
        let event_mask = (mask & 0x0000_0fff) | user_mask::IN_UNMOUNT;
        Ok((target_identity, event_mask))
    }

    fn add_watch_inner(
        &self,
        inode: Arc<dyn IndexNode>,
        mask: u32,
        event_mask: u32,
        target_identity: FsNotifyObjectId,
        object_state: Option<Arc<FsNotifyObjectState>>,
    ) -> Result<i32, SystemError> {
        // Look up an existing mark on the same inode (same group); if absent,
        // create and insert under the **same marks lock** to avoid duplicate marks
        // from concurrent add_watch on the same inode (TOCTOU: when the lookup and
        // insert are not under one lock, two threads can each pass the "no existing"
        // check and each create a mark, causing duplicate events and preventing
        // rm_watch from fully removing it).
        // Lock order marks → wd → FSNOTIFY is the nesting introduced here; no
        // reverse acquisition exists anywhere in the codebase (destroy_mark/dispatch
        // both release marks before taking wd/FSNOTIFY), so there is no deadlock.
        let mut marks = self.group.marks.lock();
        if let Some(existing) = marks.get(&target_identity) {
            if (mask & user_mask::IN_MASK_CREATE) != 0 {
                return Err(SystemError::EEXIST);
            }
            let _dispatch = existing.dispatch_lock.lock();
            if !existing.active.load(Ordering::Acquire) {
                let stale = existing.clone();
                drop(_dispatch);
                if marks
                    .get(&target_identity)
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &stale))
                {
                    marks.remove(&target_identity);
                }
                drop(marks);
                return self.add_watch_inner(
                    inode,
                    mask,
                    event_mask,
                    target_identity,
                    object_state,
                );
            }
            if (mask & user_mask::IN_MASK_ADD) != 0 {
                existing.mask.fetch_or(event_mask, Ordering::Relaxed);
                // OR semantics: oneshot takes effect if set by any source (new mask or existing state).
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

        // Allocate wd (saturating at i32::MAX-1; -1 is reserved for Q_OVERFLOW).
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
            object_state,
            object_id: target_identity,
            dispatch_lock: Mutex::new(()),
            active: AtomicBool::new(false),
            mask: AtomicU32::new(event_mask),
            oneshot: AtomicBool::new((mask & user_mask::IN_ONESHOT) != 0),
            excl_unlink: AtomicBool::new((mask & user_mask::IN_EXCL_UNLINK) != 0),
        })
        .map_err(|_| SystemError::ENOMEM)?;

        // Quota is committed only after all local allocations succeeded. Any
        // failure at the final global-index publication is rolled back below.
        reserve_watch(&self.state.quota_keys)?;
        fsnotify::adjust_total_watches(1);

        // Publish the local presence hint before the mark can become visible
        // in the global index. A concurrent removal therefore cannot observe
        // and destroy an uncharged mark.
        mark.watch_added();

        // Complete all insertions while holding the marks lock (wd table / group.marks / global index).
        self.state.wd.lock().map.insert(wd, Arc::downgrade(&mark));
        marks.insert(target_identity, mark.clone());
        // The global index insertion is the publication point. Keep the group
        // management lock held until every structure dispatch relies on is
        // complete, so there is no visible-but-half-initialized window.
        if let Err(error) = fsnotify::index_add(&mark) {
            marks.remove(&target_identity);
            self.state.wd.lock().map.remove(&wd);
            mark.watch_removed();
            fsnotify::adjust_total_watches(-1);
            release_quota(&self.state.quota_keys, 0, 1);
            return Err(error);
        }
        // The final Release publication is the watch's linearization point.
        // Every lookup path, presence hint and accounting charge is visible
        // before dispatch may observe Active.
        mark.active.store(true, Ordering::Release);
        drop(marks);
        // Note: the watch was already charged by reserve_watch (with the limit
        // check) and TOTAL_WATCHES atomically +1'd via adjust_total_watches; do
        // not +1 again here, otherwise each add would +2 while destroy only -1,
        // so TOTAL_WATCHES would never return to zero (the fast-path
        // short-circuit would break, making read/write/close hot paths
        // permanently pay the fsnotify lock overhead) and the quota limit would
        // be hit prematurely.

        Ok(wd)
    }

    /// `inotify_rm_watch`: remove a watch by wd.
    pub fn rm_watch(&self, wd: i32) -> Result<(), SystemError> {
        let mark = {
            let t = self.state.wd.lock();
            match t.map.get(&wd) {
                Some(w) => w.upgrade().ok_or(SystemError::EINVAL),
                None => Err(SystemError::EINVAL),
            }
        }?;
        // destroy_mark completes the group.marks / global index / free_mark / count cleanup.
        mark::destroy_mark(&mark)
            .then_some(())
            .ok_or(SystemError::EINVAL)
    }

    /// fd close cleanup: revoke all watches of this instance and roll back counts.
    fn shutdown(&self) {
        let marks = {
            let mut g = self.group.marks.lock();
            core::mem::take(&mut *g)
        };
        for m in marks.values() {
            if let Some(token) = FsNotifyMark::begin_retire(m, mark::RetireReason::Shutdown) {
                mark::finish_retire(token);
            }
        }
        release_quota(&self.state.quota_keys, 1, 0);
        // Wake up any thread blocked in read (the queue will no longer grow).
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
    /// An inotify fd is not seekable: pread/pwrite/lseek → ESPIPE.
    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.opened
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| SystemError::ENXIO)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // fd close: revoke all watches of this instance (no IN_IGNORED is delivered
        // — the fd no longer has a consumer), roll back the instance/global watch
        // counts, and wake up any blocked reader.
        self.shutdown();
        Ok(())
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if cmd != FIONREAD {
            return Err(SystemError::ENOIOCTLCMD);
        }

        // Snapshot under the queue lock, then release it before touching user
        // memory: a page fault must not block event producers or consumers.
        let queued_bytes = self.queued_bytes();
        debug_assert!(queued_bytes <= i32::MAX as usize);
        let queued_bytes = queued_bytes as i32;
        let mut writer =
            UserBufferWriter::new(data as *mut i32, core::mem::size_of::<i32>(), true)?;
        writer.buffer_protected(0)?.write_one(0, &queued_bytes)?;
        Ok(0)
    }

    /// read semantics: see design doc §4.3.
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        self.read_events_with(len, |written, ev| {
            let record_len = Self::record_len(ev.name.as_deref());
            Self::serialize(ev, &mut buf[written..written + record_len]);
            Ok(())
        })
    }

    fn supports_read_user(&self) -> bool {
        true
    }

    fn read_user_at(
        &self,
        _offset: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        drop(data);
        const MAX_RECORD_LEN: usize = 16 + ((NAME_MAX + 1 + 15) & !15);
        self.read_events_with(len, |written, ev| {
            let record_len = Self::record_len(ev.name.as_deref());
            let mut record = [0u8; MAX_RECORD_LEN];
            Self::serialize(ev, &mut record[..record_len]);
            writer.write_to_user(written, &record[..record_len])?;
            Ok(())
        })
        .map(Some)
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
// init implementation
// ============================================================================

/// Kernel implementation of `inotify_init1`.
pub fn do_inotify_init1(flags: u32) -> Result<usize, SystemError> {
    let flags = InotifyInitFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;

    let quota_keys = current_quota_keys()?;
    reserve_instance(&quota_keys)?;

    let nonblock = flags.contains(InotifyInitFlags::IN_NONBLOCK);
    let inode = Arc::new(InotifyInode::new(nonblock, quota_keys));
    let quota_state = inode.state.clone();
    let file_flags = FileFlags::O_RDONLY
        | (if nonblock {
            FileFlags::O_NONBLOCK
        } else {
            FileFlags::empty()
        });
    let file = File::new(inode, file_flags).inspect_err(|_| {
        // File::new failure: the file was not created, so there is no drop→shutdown; roll back the instance count manually.
        release_quota(&quota_state.quota_keys, 1, 0);
    })?;
    // Prevent recursion: read/write on the inotify fd itself must not produce events.
    file.set_mode_flags(FileMode::FMODE_NONOTIFY);

    let cloexec = flags.contains(InotifyInitFlags::IN_CLOEXEC);
    let binding = ProcessManager::current_pcb().fd_table();
    let mut fd_table_guard = binding.write();
    // On alloc_fd failure the file is dropped → File::drop → close → shutdown → roll
    // back the instance count, so there is no need to roll back manually here.
    fd_table_guard
        .alloc_fd(file, None, cloexec)
        .map(|fd| fd as usize)
}

/// `inotify_init` (no arguments, equivalent to `inotify_init1(0)`).
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

        // Fetch the InotifyInode backing the inotify fd (the file Arc keeps it alive; `inode` borrows its inode).
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
        // Resolve the path (IN_DONT_FOLLOW: do not follow the trailing symlink).
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
