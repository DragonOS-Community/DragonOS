use alloc::{collections::BTreeMap, collections::VecDeque, sync::Arc, vec, vec::Vec};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    exception::workqueue::{schedule_work, Work},
    filesystem::epoll::{
        event_poll::EventPoll, event_poll::LockedEPItemLinkedList, EPollEventType, EPollItem,
    },
    libs::{
        mutex::Mutex,
        wait_queue::{WaitQueue, Waiter},
    },
    mm::MemoryManagementArch,
    process::ProcessManager,
};

use crate::process::cred::CAPFlags;

use super::protocol::{
    fuse_pack_struct, fuse_read_struct, FuseAttrOut, FuseEntryOut, FuseForgetIn, FuseGetxattrIn,
    FuseGetxattrOut, FuseInHeader, FuseInitIn, FuseInitOut, FuseInterruptIn, FuseOpenOut,
    FuseOutHeader, FuseReadIn, FuseStatfsOut, FuseWriteIn, FuseWriteOut, FUSE_ABORT_ERROR,
    FUSE_ACCESS, FUSE_ASYNC_DIO, FUSE_ASYNC_READ, FUSE_ATOMIC_O_TRUNC, FUSE_AUTO_INVAL_DATA,
    FUSE_BIG_WRITES, FUSE_CREATE, FUSE_DESTROY, FUSE_DONT_MASK, FUSE_DO_READDIRPLUS,
    FUSE_EXPLICIT_INVAL_DATA, FUSE_EXPORT_SUPPORT, FUSE_FALLOCATE, FUSE_FLUSH, FUSE_FORGET,
    FUSE_FSYNC, FUSE_FSYNCDIR, FUSE_GETATTR, FUSE_GETXATTR, FUSE_HANDLE_KILLPRIV, FUSE_INIT,
    FUSE_INIT_EXT, FUSE_INTERRUPT, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_LINK,
    FUSE_LISTXATTR, FUSE_LOOKUP, FUSE_MAX_PAGES, FUSE_MIN_READ_BUFFER, FUSE_MKDIR, FUSE_MKNOD,
    FUSE_NOTIFY_DELETE, FUSE_NOTIFY_INVAL_ENTRY, FUSE_NOTIFY_INVAL_INODE, FUSE_NOTIFY_POLL,
    FUSE_NOTIFY_RETRIEVE, FUSE_NOTIFY_STORE, FUSE_NO_OPENDIR_SUPPORT, FUSE_NO_OPEN_SUPPORT,
    FUSE_OPEN, FUSE_OPENDIR, FUSE_PARALLEL_DIROPS, FUSE_POSIX_ACL, FUSE_POSIX_LOCKS, FUSE_READ,
    FUSE_READDIR, FUSE_READDIRPLUS, FUSE_READDIRPLUS_AUTO, FUSE_READLINK, FUSE_RELEASE,
    FUSE_RELEASEDIR, FUSE_REMOVEXATTR, FUSE_RENAME, FUSE_RENAME2, FUSE_RMDIR, FUSE_SETATTR,
    FUSE_SETXATTR, FUSE_STATFS, FUSE_SUBMOUNTS, FUSE_SYMLINK, FUSE_UNLINK, FUSE_WRITE,
};
use super::{stats, trace};

fn wait_with_recheck<T, F>(waitq: &WaitQueue, mut check: F) -> Result<T, SystemError>
where
    F: FnMut() -> Result<Option<T>, SystemError>,
{
    if let Some(v) = check()? {
        return Ok(v);
    }

    loop {
        let (waiter, waker) = Waiter::new_pair();
        waitq.register_waker(waker.clone())?;

        if let Some(v) = check()? {
            waitq.remove_waker(&waker);
            return Ok(v);
        }

        if let Err(e) = waiter.wait(true) {
            waitq.remove_waker(&waker);
            return Err(e);
        }
    }
}

#[derive(Debug)]
struct FuseBridgeWake {
    active: AtomicBool,
    events: AtomicU32,
    wait: WaitQueue,
}

impl FuseBridgeWake {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            events: AtomicU32::new(0),
            wait: WaitQueue::default(),
        }
    }

    fn install(&self) {
        self.active.store(true, Ordering::Release);
    }

    fn clear(&self) {
        self.active.store(false, Ordering::Release);
        self.events.store(0, Ordering::Release);
        self.wait.wakeup(None);
    }

    fn signal(&self, source: stats::VirtioFsBridgeWakeSource, trace_allowed: bool) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        self.events.fetch_or(source.bit(), Ordering::Release);
        stats::on_virtiofs_bridge_wake(source);
        if trace_allowed {
            trace::trace_virtiofs_bridge_wake(source.trace_id());
        }
        self.wait.wakeup(None);
    }

    fn take_events(&self) -> u32 {
        self.events.swap(0, Ordering::AcqRel)
    }

    fn events(&self) -> u32 {
        self.events.load(Ordering::Acquire)
    }

    fn wait_until<F, R>(&self, mut cond: F) -> R
    where
        F: FnMut(u32) -> Option<R>,
    {
        self.wait.wait_until(|| cond(self.events()))
    }
}

#[derive(Debug)]
pub struct FuseRequest {
    bytes: Vec<u8>,
    unique: u64,
    opcode: u32,
    reply_contract: FuseReplyContract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FuseReplyCapacitySource {
    Exact,
    RequestBounded,
    ExplicitFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FuseReplyCapacity {
    pub(crate) bytes: usize,
    pub(crate) source: FuseReplyCapacitySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FuseReplyContract {
    NoReply,
    Reply { capacity: Option<FuseReplyCapacity> },
}

impl FuseRequest {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn unique(&self) -> u64 {
        self.unique
    }

    pub(crate) fn opcode(&self) -> u32 {
        self.opcode
    }

    pub(crate) fn reply_contract(&self) -> FuseReplyContract {
        self.reply_contract
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FuseRequestCred {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

impl FuseRequestCred {
    pub(crate) fn nocreds() -> Self {
        Self {
            uid: 0,
            gid: 0,
            pid: 0,
        }
    }

    pub(crate) fn from_current() -> Self {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        let pid = pcb.task_tgid_vnr().map(|p| p.data() as u32).unwrap_or(0);
        Self {
            uid: cred.fsuid.data() as u32,
            gid: cred.fsgid.data() as u32,
            pid,
        }
    }
}

#[derive(Debug)]
pub struct FusePendingState {
    unique: u64,
    opcode: u32,
    response: Mutex<Option<Result<Vec<u8>, SystemError>>>,
    wait: WaitQueue,
}

impl FusePendingState {
    pub fn new(unique: u64, opcode: u32) -> Self {
        Self {
            unique,
            opcode,
            response: Mutex::new(None),
            wait: WaitQueue::default(),
        }
    }

    pub fn unique(&self) -> u64 {
        self.unique
    }

    pub fn complete(&self, v: Result<Vec<u8>, SystemError>) {
        let mut guard = self.response.lock();
        if guard.is_some() {
            // Duplicate replies are ignored (Linux does similarly).
            return;
        }
        *guard = Some(v);
        drop(guard);
        self.wait.wakeup(None);
    }

    pub fn wait_complete(&self) -> Result<Vec<u8>, SystemError> {
        wait_with_recheck(&self.wait, || {
            let mut guard = self.response.lock();
            if let Some(res) = guard.take() {
                return Ok(Some(res));
            }
            Ok(None)
        })?
    }
}

#[derive(Debug, Clone, Copy)]
struct FuseInitNegotiated {
    minor: u32,
    max_readahead: u32,
    max_write: u32,
    time_gran: u32,
    max_pages: u16,
    flags: u64,
}

impl Default for FuseInitNegotiated {
    fn default() -> Self {
        Self {
            minor: 0,
            max_readahead: 0,
            // Linux guarantees max_write >= 4096 after init; before init keep sane default.
            max_write: 4096,
            time_gran: 0,
            max_pages: FuseConn::DEFAULT_MAX_PAGES as u16,
            flags: 0,
        }
    }
}

#[derive(Debug)]
struct FuseConnInner {
    connected: bool,
    mounted: bool,
    initialized: bool,
    owner_uid: u32,
    owner_gid: u32,
    allow_other: bool,
    max_read: u32,
    init_flags: u64,
    init: FuseInitNegotiated,
    no_open: bool,
    no_opendir: bool,
    no_readdirplus: bool,
    no_fallocate: bool,
    no_flush: bool,
    no_fsync: bool,
    no_fsyncdir: bool,
    no_getxattr: bool,
    no_setxattr: bool,
    no_listxattr: bool,
    no_removexattr: bool,
    no_interrupt: bool,
    max_write_cap: usize,
    max_pages_limit: usize,
    separate_hiprio_pending: bool,
    teardown_started: bool,
    hiprio_pending: VecDeque<Arc<FuseRequest>>,
    pending: VecDeque<Arc<FuseRequest>>,
    processing: BTreeMap<u64, Arc<FusePendingState>>,
}

/// FUSE connection object (roughly equivalent to Linux `struct fuse_conn`).
#[derive(Debug)]
pub struct FuseConn {
    inner: Mutex<FuseConnInner>,
    next_unique: AtomicU64,
    dev_count: AtomicUsize,
    read_wait: WaitQueue,
    init_wait: WaitQueue,
    bridge_wake: FuseBridgeWake,
    epitems: LockedEPItemLinkedList,
    backend_reply_limit: Option<usize>,
    reply_layout_minor: AtomicU32,
}

impl FuseConn {
    const FUSE_INT_REQ_BIT: u64 = 1;
    // Keep this in sync with `sys_read.rs` userspace chunking size.
    const USER_READ_CHUNK: usize = 64 * 1024;
    const MIN_MAX_WRITE: usize = 4096;
    const DEFAULT_MAX_PAGES: usize = 32;
    const MAX_MAX_PAGES: usize = 256;
    const DEFAULT_MAX_READAHEAD: usize = 128 * MMArch::PAGE_SIZE;
    const XATTR_SIZE_MAX: usize = 64 * 1024;
    const FUSE_COMPAT_ENTRY_OUT_SIZE: usize = 120;
    const FUSE_COMPAT_ATTR_OUT_SIZE: usize = 96;
    const FUSE_COMPAT_STATFS_SIZE: usize = 48;
    const FUSE_COMPAT_INIT_OUT_SIZE: usize = 8;

    pub fn new() -> Arc<Self> {
        Self::new_with_max_write_cap(
            Self::max_write_cap_for_user_read_chunk(),
            Self::kernel_init_flags(),
            false,
            None,
        )
    }

    pub fn new_for_virtiofs(max_request_size: usize, max_reply_size: usize) -> Arc<Self> {
        let overhead = size_of::<FuseInHeader>() + size_of::<FuseWriteIn>();
        let cap = if max_request_size > overhead {
            core::cmp::max(Self::MIN_MAX_WRITE, max_request_size - overhead)
        } else {
            Self::MIN_MAX_WRITE
        };
        Self::new_with_max_write_cap(cap, Self::virtiofs_init_flags(), true, Some(max_reply_size))
    }

    fn new_with_max_write_cap(
        max_write_cap: usize,
        init_flags: u64,
        separate_hiprio_pending: bool,
        backend_reply_limit: Option<usize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FuseConnInner {
                connected: true,
                mounted: false,
                initialized: false,
                owner_uid: 0,
                owner_gid: 0,
                allow_other: false,
                max_read: u32::MAX,
                init_flags,
                init: FuseInitNegotiated::default(),
                no_open: false,
                no_opendir: false,
                no_readdirplus: false,
                no_fallocate: false,
                no_flush: false,
                no_fsync: false,
                no_fsyncdir: false,
                no_getxattr: false,
                no_setxattr: false,
                no_listxattr: false,
                no_removexattr: false,
                no_interrupt: false,
                max_write_cap,
                max_pages_limit: Self::MAX_MAX_PAGES,
                separate_hiprio_pending,
                teardown_started: false,
                hiprio_pending: VecDeque::new(),
                pending: VecDeque::new(),
                processing: BTreeMap::new(),
            }),
            // Use non-zero unique, keep even IDs for "ordinary" requests as Linux does.
            next_unique: AtomicU64::new(2),
            dev_count: AtomicUsize::new(1),
            read_wait: WaitQueue::default(),
            init_wait: WaitQueue::default(),
            bridge_wake: FuseBridgeWake::new(),
            epitems: LockedEPItemLinkedList::default(),
            backend_reply_limit,
            reply_layout_minor: AtomicU32::new(0),
        })
    }

    #[allow(dead_code)]
    pub fn is_mounted(&self) -> bool {
        self.inner.lock().mounted
    }

    pub fn is_connected(&self) -> bool {
        self.inner.lock().connected
    }

    pub fn has_pending_requests(&self) -> bool {
        let g = self.inner.lock();
        !g.hiprio_pending.is_empty() || !g.pending.is_empty()
    }

    pub fn has_pending_high_priority_requests(&self) -> bool {
        !self.inner.lock().hiprio_pending.is_empty()
    }

    pub fn has_pending_ordinary_requests(&self) -> bool {
        !self.inner.lock().pending.is_empty()
    }

    pub fn has_processing_request(&self, unique: u64) -> bool {
        self.inner.lock().processing.contains_key(&unique)
    }

    pub fn interrupt_target_unique(unique: u64) -> u64 {
        unique & !Self::FUSE_INT_REQ_BIT
    }

    pub fn bridge_wake_events(&self) -> u32 {
        self.bridge_wake.events()
    }

    pub fn take_bridge_wake_events(&self) -> u32 {
        self.bridge_wake.take_events()
    }

    pub fn wait_bridge_until<F, R>(&self, cond: F) -> R
    where
        F: FnMut(u32) -> Option<R>,
    {
        self.bridge_wake.wait_until(cond)
    }

    pub fn install_bridge_wake(&self) {
        self.bridge_wake.install();
    }

    pub fn clear_bridge_wake(&self) {
        self.bridge_wake.clear();
    }

    pub fn wake_bridge(&self, source: stats::VirtioFsBridgeWakeSource) {
        self.bridge_wake.signal(source, true);
    }

    pub fn wake_bridge_irq_safe(&self, source: stats::VirtioFsBridgeWakeSource) {
        self.bridge_wake.signal(source, false);
    }

    pub fn mark_mounted(&self) -> Result<(), SystemError> {
        let mut g = self.inner.lock();
        if !g.connected {
            return Err(SystemError::ENOTCONN);
        }
        if g.mounted {
            // Linux 6.6: mounting with an already-used /dev/fuse fd fails (-EINVAL).
            return Err(SystemError::EINVAL);
        }
        g.mounted = true;
        Ok(())
    }

    /// Roll back a mount reservation when mount setup fails before
    /// the filesystem is actually attached to the mount tree.
    pub fn rollback_mount_setup(&self) {
        let mut g = self.inner.lock();
        g.mounted = false;
    }

    pub fn is_initialized(&self) -> bool {
        self.inner.lock().initialized
    }

    pub fn configure_mount(
        &self,
        owner_uid: u32,
        owner_gid: u32,
        allow_other: bool,
        max_read: u32,
    ) {
        let mut g = self.inner.lock();
        g.owner_uid = owner_uid;
        g.owner_gid = owner_gid;
        g.allow_other = allow_other;
        g.max_read = core::cmp::max(Self::MIN_MAX_WRITE as u32, max_read);
    }

    pub(crate) fn has_init_flag(&self, flag: u64) -> bool {
        let g = self.inner.lock();
        (g.init.flags & flag) != 0
    }

    pub fn should_skip_open(&self, opcode: u32) -> bool {
        let g = self.inner.lock();
        match opcode {
            super::protocol::FUSE_OPEN => g.no_open,
            super::protocol::FUSE_OPENDIR => g.no_opendir,
            _ => false,
        }
    }

    pub fn open_enosys_is_supported(&self, opcode: u32) -> bool {
        match opcode {
            super::protocol::FUSE_OPEN => self.has_init_flag(FUSE_NO_OPEN_SUPPORT),
            super::protocol::FUSE_OPENDIR => self.has_init_flag(FUSE_NO_OPENDIR_SUPPORT),
            _ => false,
        }
    }

    pub fn mark_no_open(&self, opcode: u32) {
        let mut g = self.inner.lock();
        match opcode {
            super::protocol::FUSE_OPEN => g.no_open = true,
            super::protocol::FUSE_OPENDIR => g.no_opendir = true,
            _ => {}
        }
    }

    pub fn use_readdirplus(&self) -> bool {
        let g = self.inner.lock();
        !g.no_readdirplus && (g.init.flags & FUSE_DO_READDIRPLUS) != 0
    }

    pub fn disable_readdirplus(&self) {
        let mut g = self.inner.lock();
        g.no_readdirplus = true;
    }

    pub fn no_fallocate(&self) -> bool {
        self.inner.lock().no_fallocate
    }

    pub fn mark_no_fallocate(&self) {
        self.inner.lock().no_fallocate = true;
    }

    pub fn no_flush(&self) -> bool {
        self.inner.lock().no_flush
    }

    pub fn mark_no_flush(&self) {
        self.inner.lock().no_flush = true;
    }

    pub fn no_fsync(&self, opcode: u32) -> bool {
        let g = self.inner.lock();
        match opcode {
            FUSE_FSYNC => g.no_fsync,
            FUSE_FSYNCDIR => g.no_fsyncdir,
            _ => false,
        }
    }

    pub fn mark_no_fsync(&self, opcode: u32) {
        let mut g = self.inner.lock();
        match opcode {
            FUSE_FSYNC => g.no_fsync = true,
            FUSE_FSYNCDIR => g.no_fsyncdir = true,
            _ => {}
        }
    }

    pub fn no_xattr(&self, opcode: u32) -> bool {
        let g = self.inner.lock();
        match opcode {
            FUSE_GETXATTR => g.no_getxattr,
            FUSE_SETXATTR => g.no_setxattr,
            FUSE_LISTXATTR => g.no_listxattr,
            FUSE_REMOVEXATTR => g.no_removexattr,
            _ => false,
        }
    }

    pub fn mark_no_xattr(&self, opcode: u32) {
        let mut g = self.inner.lock();
        match opcode {
            FUSE_GETXATTR => g.no_getxattr = true,
            FUSE_SETXATTR => g.no_setxattr = true,
            FUSE_LISTXATTR => g.no_listxattr = true,
            FUSE_REMOVEXATTR => g.no_removexattr = true,
            _ => {}
        }
    }

    fn alloc_unique(&self) -> u64 {
        self.next_unique.fetch_add(2, Ordering::Relaxed)
    }

    fn allow_current_process(&self, cred: &crate::process::cred::Cred) -> bool {
        let g = self.inner.lock();

        if !g.mounted {
            return true;
        }

        if g.allow_other {
            return true;
        }

        // Linux: allow sysadmin to bypass allow_other restrictions (configurable).
        if cred.has_capability(CAPFlags::CAP_SYS_ADMIN) {
            return true;
        }

        let owner_uid = g.owner_uid as usize;
        let owner_gid = g.owner_gid as usize;
        cred.uid.data() == owner_uid
            && cred.euid.data() == owner_uid
            && cred.suid.data() == owner_uid
            && cred.gid.data() == owner_gid
            && cred.egid.data() == owner_gid
            && cred.sgid.data() == owner_gid
    }

    pub fn check_allow_current_process(&self) -> Result<(), SystemError> {
        let cred = ProcessManager::current_pcb().cred();
        if !self.allow_current_process(&cred) {
            return Err(SystemError::EACCES);
        }
        Ok(())
    }

    fn wait_initialized(&self) -> Result<(), SystemError> {
        wait_with_recheck(&self.init_wait, || {
            let g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if g.initialized {
                return Ok(Some(()));
            }
            Ok(None)
        })
    }

    pub fn abort(&self) {
        let (processing, pending_noreply_count): (Vec<Arc<FusePendingState>>, usize) = {
            let mut g = self.inner.lock();
            g.connected = false;
            g.mounted = false;
            let pending_noreply_count = g
                .pending
                .iter()
                .chain(g.hiprio_pending.iter())
                .filter(|req| matches!(req.opcode, FUSE_FORGET | FUSE_INTERRUPT))
                .count();
            g.hiprio_pending.clear();
            g.pending.clear();
            let processing = g.processing.values().cloned().collect();
            g.processing.clear();
            (processing, pending_noreply_count)
        };
        stats::on_fuse_requests_aborted(processing.len() + pending_noreply_count);
        for p in processing {
            p.complete(Err(SystemError::ENOTCONN));
        }
        self.read_wait.wakeup(None);
        self.wake_bridge(stats::VirtioFsBridgeWakeSource::Disconnect);
        self.init_wait.wakeup(None);
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLERR | EPollEventType::EPOLLHUP,
        );
    }

    /// Unmount path: fail in-flight requests and best-effort queue DESTROY.
    ///
    /// Keep the connection readable for daemon-side teardown; actual disconnect
    /// happens when /dev/fuse is closed or explicit abort path is triggered.
    pub fn on_umount(&self) {
        let processing: Vec<Arc<FusePendingState>>;
        let dropped_processing: Vec<Arc<FusePendingState>>;
        let should_destroy: bool;
        let dropped_pending: usize;
        {
            let mut g = self.inner.lock();
            if g.teardown_started {
                return;
            }
            g.teardown_started = true;
            should_destroy = g.connected && g.initialized;
            g.mounted = false;
            // Filesystem teardown queues accumulated FORGET requests before
            // the connection enters on_umount().  Preserve those no-reply
            // requests so the daemon can release lookup references before
            // it receives DESTROY; drop ordinary requests that can no longer
            // complete after unmount.
            let mut dropped_noreply = 0usize;
            let mut dropped_reply_unique = Vec::new();
            for req in g.hiprio_pending.iter().chain(g.pending.iter()) {
                if req.opcode == FUSE_FORGET {
                    continue;
                }
                if matches!(req.opcode, FUSE_DESTROY | FUSE_INTERRUPT) {
                    dropped_noreply += 1;
                } else if g.processing.contains_key(&req.unique) {
                    dropped_reply_unique.push(req.unique);
                }
            }
            g.hiprio_pending.retain(|req| req.opcode == FUSE_FORGET);
            g.pending.retain(|req| req.opcode == FUSE_FORGET);
            dropped_processing = dropped_reply_unique
                .into_iter()
                .filter_map(|unique| g.processing.remove(&unique))
                .collect();
            dropped_pending = dropped_noreply + dropped_processing.len();
            processing = g.processing.values().cloned().collect();
            g.processing.clear();
        }

        stats::on_fuse_requests_dropped_umount(dropped_pending);
        stats::on_fuse_requests_aborted(processing.len());
        for p in processing {
            p.complete(Err(SystemError::ENOTCONN));
        }
        for p in dropped_processing {
            p.complete(Err(SystemError::ENOTCONN));
        }
        self.init_wait.wakeup(None);

        if !should_destroy {
            self.abort();
            return;
        }

        if self
            .enqueue_request_with_cred(FUSE_DESTROY, 0, &[], FuseRequestCred::nocreds())
            .is_err()
        {
            self.abort();
            return;
        }
        self.wake_bridge(stats::VirtioFsBridgeWakeSource::Teardown);
    }

    /// Queue a FORGET message (no reply expected).
    pub fn queue_forget(&self, nodeid: u64, nlookup: u64) -> Result<(), SystemError> {
        if nodeid == 0 || nlookup == 0 {
            return Ok(());
        }
        let can_send = {
            let g = self.inner.lock();
            g.connected && g.mounted && g.initialized
        };
        if !can_send {
            return Ok(());
        }
        let inarg = FuseForgetIn { nlookup };
        self.enqueue_noreply(FUSE_FORGET, nodeid, fuse_pack_struct(&inarg))
    }

    fn queue_interrupt(&self, unique: u64) -> Result<(), SystemError> {
        if unique == 0 {
            return Ok(());
        }
        let can_send = {
            let g = self.inner.lock();
            g.connected
                && g.mounted
                && g.initialized
                && !g.no_interrupt
                && g.processing.contains_key(&unique)
        };
        if !can_send {
            return Ok(());
        }
        let inarg = FuseInterruptIn { unique };
        let req = self.build_request(
            unique | Self::FUSE_INT_REQ_BIT,
            FUSE_INTERRUPT,
            0,
            fuse_pack_struct(&inarg),
            FuseRequestCred::nocreds(),
            false,
        )?;
        self.push_request(req, None, unique | Self::FUSE_INT_REQ_BIT)
    }

    /// Acquire a new `/dev/fuse` file handle reference to this connection.
    pub fn dev_acquire(&self) {
        self.dev_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Release a `/dev/fuse` file handle reference. When the last handle is closed,
    /// abort the connection (Linux: daemon exits).
    pub fn dev_release(&self) {
        if self.dev_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.abort();
        }
    }

    pub fn poll_mask(&self, have_pending: bool) -> EPollEventType {
        let mut events = EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
        let g = self.inner.lock();
        if !g.connected {
            return EPollEventType::EPOLLERR;
        }
        if have_pending {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        events
    }

    pub fn poll(&self) -> EPollEventType {
        let g = self.inner.lock();
        let have_pending = !g.hiprio_pending.is_empty() || !g.pending.is_empty();
        drop(g);
        self.poll_mask(have_pending)
    }

    pub fn add_epitem(&self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        self.epitems.lock().push_back(epitem);
        Ok(())
    }

    pub fn remove_epitem(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        let mut guard = self.epitems.lock();
        let len = guard.len();
        guard.retain(|x| !Arc::ptr_eq(x, epitem));
        if len != guard.len() {
            return Ok(());
        }
        Err(SystemError::ENOENT)
    }

    fn calc_min_read_buffer(max_write: usize) -> usize {
        core::cmp::max(
            FUSE_MIN_READ_BUFFER,
            core::mem::size_of::<FuseInHeader>() + core::mem::size_of::<FuseWriteIn>() + max_write,
        )
    }

    fn max_write_cap_for_user_read_chunk() -> usize {
        let overhead = core::mem::size_of::<FuseInHeader>() + core::mem::size_of::<FuseWriteIn>();
        if Self::USER_READ_CHUNK <= overhead {
            Self::MIN_MAX_WRITE
        } else {
            Self::USER_READ_CHUNK - overhead
        }
    }

    pub fn set_max_pages_limit(&self, max_pages_limit: usize) -> Result<(), SystemError> {
        if max_pages_limit == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut g = self.inner.lock();
        if g.initialized {
            return Err(SystemError::EBUSY);
        }
        g.max_pages_limit = core::cmp::min(max_pages_limit, Self::MAX_MAX_PAGES);
        Ok(())
    }

    fn min_read_buffer(&self) -> usize {
        let g = self.inner.lock();
        Self::calc_min_read_buffer(g.init.max_write as usize)
    }

    fn pop_pending_nonblock(&self) -> Result<Arc<FuseRequest>, SystemError> {
        let mut g = self.inner.lock();
        if !g.connected {
            return Err(SystemError::ENOTCONN);
        }
        Self::pop_pending_locked(&mut g).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn pop_pending_blocking(&self) -> Result<Arc<FuseRequest>, SystemError> {
        wait_with_recheck(&self.read_wait, || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if let Some(req) = Self::pop_pending_locked(&mut g) {
                return Ok(Some(req));
            }
            Ok(None)
        })
    }

    fn pop_ordinary_pending_nonblock(&self) -> Result<Arc<FuseRequest>, SystemError> {
        let mut g = self.inner.lock();
        if !g.connected {
            return Err(SystemError::ENOTCONN);
        }
        g.pending
            .pop_front()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn pop_ordinary_pending_blocking(&self) -> Result<Arc<FuseRequest>, SystemError> {
        wait_with_recheck(&self.read_wait, || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if let Some(req) = g.pending.pop_front() {
                return Ok(Some(req));
            }
            Ok(None)
        })
    }

    fn is_high_priority_opcode(opcode: u32) -> bool {
        matches!(opcode, FUSE_FORGET | FUSE_INTERRUPT)
    }

    fn is_stale_interrupt_locked(req: &FuseRequest, g: &FuseConnInner) -> bool {
        req.opcode == FUSE_INTERRUPT
            && !g
                .processing
                .contains_key(&Self::interrupt_target_unique(req.unique))
    }

    fn pop_high_priority_pending_locked(g: &mut FuseConnInner) -> Option<Arc<FuseRequest>> {
        loop {
            let req = g.hiprio_pending.pop_front()?;
            if Self::is_stale_interrupt_locked(&req, g) {
                stats::on_fuse_requests_aborted(1);
                continue;
            }
            return Some(req);
        }
    }

    fn pop_pending_locked(g: &mut FuseConnInner) -> Option<Arc<FuseRequest>> {
        Self::pop_high_priority_pending_locked(g).or_else(|| g.pending.pop_front())
    }

    fn complete_dequeued_request(
        &self,
        req: Arc<FuseRequest>,
        out: &mut [u8],
    ) -> Result<usize, SystemError> {
        out[..req.bytes.len()].copy_from_slice(&req.bytes);
        self.account_dequeued_request(&req);
        Ok(req.bytes.len())
    }

    fn account_dequeued_request(&self, req: &FuseRequest) {
        stats::on_fuse_request_dequeued(req.bytes.len());
        trace::trace_fuse_request_dequeue(req.unique, req.opcode, req.bytes.len() as u64);
    }

    fn dequeue_for_kernel_transport<F>(
        &self,
        max_message_size: usize,
        pop_request: F,
    ) -> Result<Arc<FuseRequest>, SystemError>
    where
        F: FnOnce() -> Result<Arc<FuseRequest>, SystemError>,
    {
        let req = pop_request()?;
        if req.bytes.len() > max_message_size {
            self.fail_oversized_read_request(&req);
            log::warn!(
                "fuse: kernel transport buffer smaller than queued request: got={} need={}",
                max_message_size,
                req.bytes.len()
            );
            // Return to the bridge loop after one malformed request so completion, reset and the
            // other priority queue cannot be starved by a producer of oversized requests.
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        self.account_dequeued_request(&req);
        Ok(req)
    }

    pub(crate) fn dequeue_virtiofs_ordinary_request(
        &self,
        max_message_size: usize,
    ) -> Result<Arc<FuseRequest>, SystemError> {
        self.dequeue_for_kernel_transport(max_message_size, || self.pop_ordinary_pending_nonblock())
    }

    pub(crate) fn dequeue_virtiofs_high_priority_request(
        &self,
        max_message_size: usize,
    ) -> Result<Arc<FuseRequest>, SystemError> {
        self.dequeue_for_kernel_transport(max_message_size, || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            Self::pop_high_priority_pending_locked(&mut g).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
        })
    }

    fn read_dequeued_request<F>(
        &self,
        nonblock: bool,
        out: &mut [u8],
        context: &str,
        mut pop_request: F,
    ) -> Result<usize, SystemError>
    where
        F: FnMut() -> Result<Arc<FuseRequest>, SystemError>,
    {
        let min_read = self.min_read_buffer();
        if out.len() < min_read {
            log::warn!(
                "fuse: read buffer too small for {}: got={} min={} nonblock={}",
                context,
                out.len(),
                min_read,
                nonblock
            );
            return Err(SystemError::EINVAL);
        }

        let req = loop {
            let req = pop_request()?;

            if out.len() >= req.bytes.len() {
                break req;
            }

            self.fail_oversized_read_request(&req);
            log::warn!(
                "fuse: read buffer smaller than queued {} request: got={} need={}",
                context,
                out.len(),
                req.bytes.len()
            );
        };

        self.complete_dequeued_request(req, out)
    }

    pub fn read_request(&self, nonblock: bool, out: &mut [u8]) -> Result<usize, SystemError> {
        // Linux: require a sane minimum read buffer for all reads.
        self.read_dequeued_request(nonblock, out, "request", || {
            // Linux: if O_NONBLOCK and no pending request, return EAGAIN.
            if nonblock {
                self.pop_pending_nonblock()
            } else {
                self.pop_pending_blocking()
            }
        })
    }

    pub fn read_ordinary_request(
        &self,
        nonblock: bool,
        out: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.read_dequeued_request(nonblock, out, "ordinary request", || {
            if nonblock {
                self.pop_ordinary_pending_nonblock()
            } else {
                self.pop_ordinary_pending_blocking()
            }
        })
    }

    pub fn read_high_priority_request(&self, out: &mut [u8]) -> Result<usize, SystemError> {
        self.read_dequeued_request(true, out, "high-priority request", || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            Self::pop_high_priority_pending_locked(&mut g).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
        })
    }

    fn fail_oversized_read_request(&self, req: &FuseRequest) {
        stats::on_fuse_read_buffer_too_small();
        let err = if req.opcode == FUSE_SETXATTR {
            SystemError::E2BIG
        } else {
            SystemError::EIO
        };
        let pending = {
            let mut g = self.inner.lock();
            g.processing.remove(&req.unique)
        };

        if let Some(pending) = pending {
            let error = -err.to_posix_errno();
            stats::on_fuse_reply_complete(req.opcode, error, 0);
            trace::trace_fuse_reply_complete(req.unique, req.opcode, error, 0);
            pending.complete(Err(err));
        } else {
            stats::on_fuse_requests_aborted(1);
        }
    }

    fn kernel_init_flags() -> u64 {
        FUSE_ASYNC_READ
            | FUSE_POSIX_LOCKS
            | FUSE_ATOMIC_O_TRUNC
            | FUSE_EXPORT_SUPPORT
            | FUSE_BIG_WRITES
            | FUSE_DONT_MASK
            | FUSE_AUTO_INVAL_DATA
            | FUSE_DO_READDIRPLUS
            | FUSE_READDIRPLUS_AUTO
            | FUSE_ASYNC_DIO
            | FUSE_NO_OPEN_SUPPORT
            | FUSE_NO_OPENDIR_SUPPORT
            | FUSE_PARALLEL_DIROPS
            | FUSE_HANDLE_KILLPRIV
            | FUSE_POSIX_ACL
            | FUSE_ABORT_ERROR
            | FUSE_MAX_PAGES
            | FUSE_EXPLICIT_INVAL_DATA
            | FUSE_INIT_EXT
    }

    /// virtiofs uses the normal FUSE capability request plus Linux's submount bit.
    /// WRITEBACK_CACHE is not requested until DragonOS has complete writeback-cache semantics.
    fn virtiofs_init_flags() -> u64 {
        Self::kernel_init_flags() | FUSE_SUBMOUNTS
    }

    pub fn supports_submounts(&self) -> bool {
        self.has_init_flag(FUSE_SUBMOUNTS)
    }

    pub fn enqueue_init(&self) -> Result<(), SystemError> {
        let flags = self.inner.lock().init_flags;
        let init_in = FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: Self::DEFAULT_MAX_READAHEAD as u32,
            flags: flags as u32,
            flags2: (flags >> 32) as u32,
            unused: [0; 11],
        };
        self.enqueue_request(FUSE_INIT, 0, fuse_pack_struct(&init_in))
            .map(|_| ())
    }

    pub fn request(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<Vec<u8>, SystemError> {
        if opcode != FUSE_INIT {
            let cred = ProcessManager::current_pcb().cred();
            if !self.allow_current_process(&cred) {
                return Err(SystemError::EACCES);
            }
            self.wait_initialized()?;
        }
        let pending = self.enqueue_request(opcode, nodeid, payload)?;
        self.wait_request_complete(opcode, pending)
    }

    pub fn request_nocreds(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<Vec<u8>, SystemError> {
        if opcode != FUSE_INIT {
            self.wait_initialized()?;
        }
        let pending =
            self.enqueue_request_with_cred(opcode, nodeid, payload, FuseRequestCred::nocreds())?;
        self.wait_request_complete(opcode, pending)
    }

    pub(crate) fn request_with_cred(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
        req_cred: FuseRequestCred,
    ) -> Result<Vec<u8>, SystemError> {
        if opcode != FUSE_INIT {
            self.wait_initialized()?;
        }
        let pending = self.enqueue_request_with_cred(opcode, nodeid, payload, req_cred)?;
        self.wait_request_complete(opcode, pending)
    }

    pub fn request_nocreds_background(
        self: &Arc<Self>,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<(), SystemError> {
        if opcode != FUSE_INIT {
            self.wait_initialized()?;
        }
        let pending =
            self.enqueue_request_with_cred(opcode, nodeid, payload, FuseRequestCred::nocreds())?;
        let conn = self.clone();
        schedule_work(Work::new(move || {
            if let Err(err) = conn.wait_request_complete(opcode, pending.clone()) {
                if matches!(err, SystemError::ENOTCONN | SystemError::ENOENT) {
                    log::debug!(
                        "fuse: background request aborted opcode={} nodeid={} err={:?}",
                        opcode,
                        nodeid,
                        err
                    );
                } else {
                    log::warn!(
                        "fuse: background request failed opcode={} nodeid={} err={:?}",
                        opcode,
                        nodeid,
                        err
                    );
                }
            }
        }));
        Ok(())
    }

    fn wait_request_complete(
        &self,
        opcode: u32,
        pending: Arc<FusePendingState>,
    ) -> Result<Vec<u8>, SystemError> {
        match pending.wait_complete() {
            Err(SystemError::EINTR) | Err(SystemError::ERESTARTSYS) => {
                if opcode != FUSE_INTERRUPT {
                    let _ = self.queue_interrupt(pending.unique());
                }
                Err(SystemError::EINTR)
            }
            x => x,
        }
    }

    fn enqueue_noreply(&self, opcode: u32, nodeid: u64, payload: &[u8]) -> Result<(), SystemError> {
        debug_assert_eq!(opcode, FUSE_FORGET);
        let unique = self.alloc_unique();
        let req = self.build_request(
            unique,
            opcode,
            nodeid,
            payload,
            FuseRequestCred {
                uid: 0,
                gid: 0,
                pid: 0,
            },
            true,
        )?;
        self.push_request(req, None, unique)?;
        Ok(())
    }

    fn enqueue_request(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<Arc<FusePendingState>, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        if !self.allow_current_process(&cred) {
            return Err(SystemError::EACCES);
        }
        self.enqueue_request_with_cred(opcode, nodeid, payload, FuseRequestCred::from_current())
    }

    fn enqueue_request_with_cred(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
        req_cred: FuseRequestCred,
    ) -> Result<Arc<FusePendingState>, SystemError> {
        let unique = self.alloc_unique();
        let pending_state = Arc::new(FusePendingState::new(unique, opcode));

        let req = self.build_request(unique, opcode, nodeid, payload, req_cred, false)?;
        self.push_request(req, Some(pending_state.clone()), unique)?;
        Ok(pending_state)
    }

    fn reply_capacity(
        &self,
        opcode: u32,
        payload: &[u8],
    ) -> Result<Option<FuseReplyCapacity>, SystemError> {
        let minor = self.reply_layout_minor.load(Ordering::Acquire);
        let header = size_of::<FuseOutHeader>();
        let exact = |payload_len: usize| -> Result<Option<FuseReplyCapacity>, SystemError> {
            let bytes = header
                .checked_add(payload_len)
                .ok_or(SystemError::EOVERFLOW)?;
            if self.backend_reply_limit.is_some_and(|limit| bytes > limit) {
                return Err(SystemError::EOVERFLOW);
            }
            Ok(Some(FuseReplyCapacity {
                bytes,
                source: FuseReplyCapacitySource::Exact,
            }))
        };

        let entry_size = if minor < 9 {
            Self::FUSE_COMPAT_ENTRY_OUT_SIZE
        } else {
            size_of::<FuseEntryOut>()
        };
        let attr_size = if minor < 9 {
            Self::FUSE_COMPAT_ATTR_OUT_SIZE
        } else {
            size_of::<FuseAttrOut>()
        };

        let fixed_payload = match opcode {
            FUSE_LOOKUP | FUSE_SYMLINK | FUSE_MKNOD | FUSE_MKDIR | FUSE_LINK => Some(entry_size),
            FUSE_GETATTR | FUSE_SETATTR => Some(attr_size),
            FUSE_OPEN | FUSE_OPENDIR => Some(size_of::<FuseOpenOut>()),
            FUSE_WRITE => Some(size_of::<FuseWriteOut>()),
            FUSE_STATFS => Some(if minor < 4 {
                Self::FUSE_COMPAT_STATFS_SIZE
            } else {
                size_of::<FuseStatfsOut>()
            }),
            FUSE_CREATE => Some(
                entry_size
                    .checked_add(size_of::<FuseOpenOut>())
                    .ok_or(SystemError::EOVERFLOW)?,
            ),
            FUSE_INIT => Some(size_of::<FuseInitOut>()),
            FUSE_UNLINK | FUSE_RMDIR | FUSE_RENAME | FUSE_RENAME2 | FUSE_RELEASE | FUSE_FSYNC
            | FUSE_SETXATTR | FUSE_REMOVEXATTR | FUSE_FLUSH | FUSE_RELEASEDIR | FUSE_FSYNCDIR
            | FUSE_ACCESS | FUSE_INTERRUPT | FUSE_DESTROY | FUSE_FALLOCATE => Some(0),
            _ => None,
        };
        if let Some(payload_len) = fixed_payload {
            return exact(payload_len);
        }

        let requested = match opcode {
            FUSE_READ | FUSE_READDIR | FUSE_READDIRPLUS => {
                let read_in: FuseReadIn = fuse_read_struct(payload)?;
                let requested = read_in.size as usize;
                let (max_read, max_pages) = {
                    let g = self.inner.lock();
                    (g.max_read as usize, g.init.max_pages as usize)
                };
                let max_pages_bytes = max_pages
                    .checked_mul(MMArch::PAGE_SIZE)
                    .ok_or(SystemError::EOVERFLOW)?;
                if requested > max_read || requested > max_pages_bytes {
                    return Err(SystemError::EINVAL);
                }
                requested
            }
            FUSE_GETXATTR | FUSE_LISTXATTR => {
                let getxattr_in: FuseGetxattrIn = fuse_read_struct(payload)?;
                let requested = getxattr_in.size as usize;
                if requested > Self::XATTR_SIZE_MAX {
                    return Err(SystemError::EINVAL);
                }
                if requested == 0 {
                    size_of::<FuseGetxattrOut>()
                } else {
                    requested
                }
            }
            FUSE_READLINK => MMArch::PAGE_SIZE
                .checked_sub(1)
                .ok_or(SystemError::EOVERFLOW)?,
            _ => {
                return match self.backend_reply_limit {
                    Some(bytes) if bytes >= header => Ok(Some(FuseReplyCapacity {
                        bytes,
                        source: FuseReplyCapacitySource::ExplicitFallback,
                    })),
                    Some(_) => Err(SystemError::EOVERFLOW),
                    None => Ok(None),
                };
            }
        };

        let bytes = header
            .checked_add(requested)
            .ok_or(SystemError::EOVERFLOW)?;
        if self.backend_reply_limit.is_some_and(|limit| bytes > limit) {
            return Err(SystemError::EOVERFLOW);
        }
        Ok(Some(FuseReplyCapacity {
            bytes,
            source: FuseReplyCapacitySource::RequestBounded,
        }))
    }

    fn build_request(
        &self,
        unique: u64,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
        req_cred: FuseRequestCred,
        no_reply: bool,
    ) -> Result<Arc<FuseRequest>, SystemError> {
        let request_len = size_of::<FuseInHeader>()
            .checked_add(payload.len())
            .ok_or(SystemError::EOVERFLOW)?;
        let request_len = u32::try_from(request_len).map_err(|_| SystemError::EOVERFLOW)?;
        let reply_contract = if no_reply {
            FuseReplyContract::NoReply
        } else {
            FuseReplyContract::Reply {
                capacity: self.reply_capacity(opcode, payload)?,
            }
        };
        let hdr = FuseInHeader {
            len: request_len,
            opcode,
            unique,
            nodeid,
            uid: req_cred.uid,
            gid: req_cred.gid,
            pid: req_cred.pid,
            total_extlen: 0,
            padding: 0,
        };

        let mut bytes = Vec::with_capacity(hdr.len as usize);
        bytes.extend_from_slice(fuse_pack_struct(&hdr));
        bytes.extend_from_slice(payload);
        Ok(Arc::new(FuseRequest {
            bytes,
            unique,
            opcode,
            reply_contract,
        }))
    }

    fn push_request(
        &self,
        req: Arc<FuseRequest>,
        pending_state: Option<Arc<FusePendingState>>,
        unique: u64,
    ) -> Result<(), SystemError> {
        let req_len = req.bytes.len();
        let opcode = req.opcode;
        let no_reply = matches!(req.reply_contract, FuseReplyContract::NoReply);
        debug_assert_eq!(
            no_reply,
            pending_state.is_none() && opcode != FUSE_INTERRUPT
        );
        {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if g.teardown_started && opcode != FUSE_DESTROY {
                return Err(SystemError::ENOTCONN);
            }
            if g.separate_hiprio_pending && Self::is_high_priority_opcode(opcode) {
                g.hiprio_pending.push_back(req);
            } else {
                g.pending.push_back(req);
            }
            if let Some(pending) = pending_state {
                g.processing.insert(unique, pending);
            }
        }

        stats::on_fuse_request_queued(req_len, no_reply);
        trace::trace_fuse_request_queue(unique, opcode, req_len as u64, no_reply as u8);
        self.read_wait.wakeup(None);
        self.wake_bridge(stats::VirtioFsBridgeWakeSource::Request);
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
        Ok(())
    }

    fn parse_init_reply(payload: &[u8]) -> Result<FuseInitOut, SystemError> {
        if payload.len() < Self::FUSE_COMPAT_INIT_OUT_SIZE
            || payload.len() > size_of::<FuseInitOut>()
        {
            return Err(SystemError::EINVAL);
        }
        let mut normalized = vec![0u8; size_of::<FuseInitOut>()];
        normalized[..payload.len()].copy_from_slice(payload);
        fuse_read_struct(&normalized)
    }

    fn normalize_compat_reply(
        minor: u32,
        opcode: u32,
        payload: &[u8],
    ) -> Result<Vec<u8>, SystemError> {
        let (compat_len, full_len) = if minor < 4 && opcode == FUSE_STATFS {
            (Self::FUSE_COMPAT_STATFS_SIZE, size_of::<FuseStatfsOut>())
        } else if minor < 9
            && matches!(
                opcode,
                FUSE_LOOKUP | FUSE_SYMLINK | FUSE_MKNOD | FUSE_MKDIR | FUSE_LINK
            )
        {
            (Self::FUSE_COMPAT_ENTRY_OUT_SIZE, size_of::<FuseEntryOut>())
        } else if minor < 9 && matches!(opcode, FUSE_GETATTR | FUSE_SETATTR) {
            (Self::FUSE_COMPAT_ATTR_OUT_SIZE, size_of::<FuseAttrOut>())
        } else if minor < 9 && opcode == FUSE_CREATE {
            let compat_len = Self::FUSE_COMPAT_ENTRY_OUT_SIZE
                .checked_add(size_of::<FuseOpenOut>())
                .ok_or(SystemError::EOVERFLOW)?;
            if payload.len() != compat_len {
                return Err(SystemError::EINVAL);
            }
            let mut normalized = vec![0u8; size_of::<FuseEntryOut>() + size_of::<FuseOpenOut>()];
            normalized[..Self::FUSE_COMPAT_ENTRY_OUT_SIZE]
                .copy_from_slice(&payload[..Self::FUSE_COMPAT_ENTRY_OUT_SIZE]);
            normalized[size_of::<FuseEntryOut>()..]
                .copy_from_slice(&payload[Self::FUSE_COMPAT_ENTRY_OUT_SIZE..compat_len]);
            return Ok(normalized);
        } else {
            return Ok(payload.to_vec());
        };

        if payload.len() != compat_len {
            return Err(SystemError::EINVAL);
        }
        let mut normalized = vec![0u8; full_len];
        normalized[..compat_len].copy_from_slice(payload);
        Ok(normalized)
    }

    pub fn write_reply(&self, data: &[u8]) -> Result<usize, SystemError> {
        if data.len() < core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }

        let out_hdr: FuseOutHeader = fuse_read_struct(data)?;
        if out_hdr.len as usize != data.len() {
            return Err(SystemError::EINVAL);
        }

        if out_hdr.unique == 0 {
            let payload = &data[core::mem::size_of::<FuseOutHeader>()..];
            self.handle_notify(out_hdr.error, payload)?;
            return Ok(data.len());
        }

        if (out_hdr.unique & Self::FUSE_INT_REQ_BIT) != 0 {
            return self.write_interrupt_reply(&out_hdr, data.len());
        }

        if out_hdr.error <= -512 || out_hdr.error > 0 {
            return Err(SystemError::EINVAL);
        }
        if out_hdr.error != 0 && data.len() != core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }

        let (pending, negotiated_minor) = {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOENT);
            }
            let pending = g
                .processing
                .remove(&out_hdr.unique)
                .ok_or(SystemError::ENOENT)?;
            (pending, g.init.minor)
        };

        let payload = &data[core::mem::size_of::<FuseOutHeader>()..];
        let error = out_hdr.error;

        if error != 0 {
            // Negative errno from userspace.
            let errno = -error;
            let e = SystemError::from_i32(errno).unwrap_or(SystemError::EIO);
            if Self::is_expected_reply_error(pending.opcode, errno) {
                log::trace!(
                    "fuse: reply error opcode={} unique={} errno={}",
                    pending.opcode,
                    out_hdr.unique,
                    errno
                );
            } else {
                log::warn!(
                    "fuse: reply error opcode={} unique={} errno={}",
                    pending.opcode,
                    out_hdr.unique,
                    errno
                );
            }
            pending.complete(Err(e));
            stats::on_fuse_reply_complete(pending.opcode, error, payload.len());
            trace::trace_fuse_reply_complete(
                out_hdr.unique,
                pending.opcode,
                error,
                payload.len() as u64,
            );
            if matches!(pending.opcode, FUSE_INIT | FUSE_DESTROY) {
                self.abort();
            }
            return Ok(data.len());
        }

        if pending.opcode == FUSE_INIT {
            let init_out: FuseInitOut = match Self::parse_init_reply(payload) {
                Ok(v) => v,
                Err(e) => {
                    let error = -e.to_posix_errno();
                    stats::on_fuse_reply_complete(pending.opcode, error, payload.len());
                    trace::trace_fuse_reply_complete(
                        out_hdr.unique,
                        pending.opcode,
                        error,
                        payload.len() as u64,
                    );
                    pending.complete(Err(e));
                    self.abort();
                    return Ok(data.len());
                }
            };

            if init_out.major != FUSE_KERNEL_VERSION {
                let error = -SystemError::EINVAL.to_posix_errno();
                stats::on_fuse_reply_complete(pending.opcode, error, payload.len());
                trace::trace_fuse_reply_complete(
                    out_hdr.unique,
                    pending.opcode,
                    error,
                    payload.len() as u64,
                );
                pending.complete(Err(SystemError::EINVAL));
                self.abort();
                return Ok(data.len());
            }

            let mut negotiated_flags = init_out.flags as u64;
            if (negotiated_flags & FUSE_INIT_EXT) != 0 {
                negotiated_flags |= (init_out.flags2 as u64) << 32;
            }
            let negotiated_minor = core::cmp::min(init_out.minor, FUSE_KERNEL_MINOR_VERSION);
            let negotiated_max_pages_raw = if (negotiated_flags & FUSE_MAX_PAGES) != 0 {
                core::cmp::max(init_out.max_pages, 1)
            } else {
                Self::DEFAULT_MAX_PAGES as u16
            };
            let negotiated_max_write =
                core::cmp::max(Self::MIN_MAX_WRITE, init_out.max_write as usize);
            let (max_write_cap, max_pages_limit) = {
                let g = self.inner.lock();
                (
                    core::cmp::max(Self::MIN_MAX_WRITE, g.max_write_cap),
                    g.max_pages_limit,
                )
            };
            let capped_max_write = core::cmp::min(negotiated_max_write, max_write_cap);
            if capped_max_write < negotiated_max_write {
                log::trace!(
                    "fuse: cap negotiated max_write from {} to {} due backend read buffer limit",
                    negotiated_max_write,
                    capped_max_write
                );
            }
            let negotiated_max_pages =
                core::cmp::min(negotiated_max_pages_raw, max_pages_limit as u16);

            {
                let mut g = self.inner.lock();
                if g.connected {
                    // Align with Linux: only enable feature bits that are in the intersection of
                    // daemon-supported and locally-requested flags. virtiofsd often sets
                    // DO_READDIRPLUS etc. in the INIT reply; if adopted directly, it would
                    // trigger READDIRPLUS + cache_child_from_entry, causing stale inode mappings.
                    let enabled_flags = negotiated_flags & g.init_flags;
                    g.initialized = true;
                    g.init = FuseInitNegotiated {
                        minor: negotiated_minor,
                        max_readahead: init_out.max_readahead,
                        max_write: capped_max_write as u32,
                        time_gran: init_out.time_gran,
                        max_pages: negotiated_max_pages,
                        flags: enabled_flags,
                    };
                    self.reply_layout_minor
                        .store(negotiated_minor, Ordering::Release);
                }
            }
            self.init_wait.wakeup(None);
        }

        let normalized_payload = if pending.opcode == FUSE_INIT {
            payload.to_vec()
        } else {
            match Self::normalize_compat_reply(negotiated_minor, pending.opcode, payload) {
                Ok(payload) => payload,
                Err(e) => {
                    let error = -e.to_posix_errno();
                    stats::on_fuse_reply_complete(pending.opcode, error, payload.len());
                    trace::trace_fuse_reply_complete(
                        out_hdr.unique,
                        pending.opcode,
                        error,
                        payload.len() as u64,
                    );
                    pending.complete(Err(e));
                    if pending.opcode == FUSE_DESTROY {
                        self.abort();
                    }
                    return Ok(data.len());
                }
            }
        };

        stats::on_fuse_reply_complete(pending.opcode, 0, payload.len());
        trace::trace_fuse_reply_complete(out_hdr.unique, pending.opcode, 0, payload.len() as u64);
        pending.complete(Ok(normalized_payload));
        if pending.opcode == FUSE_DESTROY {
            self.abort();
        }
        Ok(data.len())
    }

    /// Linux supplies an output header buffer for DESTROY, while virtiofsd completes the
    /// descriptor with zero used bytes. Linux treats the zero-initialized request header as a
    /// successful completion because DESTROY has no output arguments. Preserve that transport
    /// semantic without relaxing validation for any other opcode.
    pub(crate) fn complete_destroy_without_reply(&self, unique: u64) -> Result<(), SystemError> {
        let pending = {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOENT);
            }
            let pending = g.processing.remove(&unique).ok_or(SystemError::ENOENT)?;
            if pending.opcode != FUSE_DESTROY {
                g.processing.insert(unique, pending);
                return Err(SystemError::EINVAL);
            }
            pending
        };

        stats::on_fuse_reply_complete(FUSE_DESTROY, 0, 0);
        trace::trace_fuse_reply_complete(unique, FUSE_DESTROY, 0, 0);
        pending.complete(Ok(Vec::new()));
        self.abort();
        Ok(())
    }

    fn write_interrupt_reply(
        &self,
        out_hdr: &FuseOutHeader,
        data_len: usize,
    ) -> Result<usize, SystemError> {
        if data_len != core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }
        if out_hdr.error <= -512 || out_hdr.error > 0 {
            return Err(SystemError::EINVAL);
        }

        let target_unique = out_hdr.unique & !Self::FUSE_INT_REQ_BIT;
        {
            let mut g = self.inner.lock();
            if !g.connected || !g.processing.contains_key(&target_unique) {
                return Err(SystemError::ENOENT);
            }
            if out_hdr.error == -SystemError::ENOSYS.to_posix_errno() {
                g.no_interrupt = true;
            }
        }

        if out_hdr.error == -SystemError::EAGAIN_OR_EWOULDBLOCK.to_posix_errno() {
            self.queue_interrupt(target_unique)?;
        }

        Ok(data_len)
    }

    fn handle_notify(&self, code: i32, payload: &[u8]) -> Result<(), SystemError> {
        if code <= 0 {
            return Err(SystemError::EINVAL);
        }
        match code {
            FUSE_NOTIFY_POLL
            | FUSE_NOTIFY_INVAL_INODE
            | FUSE_NOTIFY_INVAL_ENTRY
            | FUSE_NOTIFY_STORE
            | FUSE_NOTIFY_RETRIEVE
            | FUSE_NOTIFY_DELETE => {
                log::debug!("fuse: notify code={} len={}", code, payload.len());
                Ok(())
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn is_expected_reply_error(opcode: u32, errno: i32) -> bool {
        matches!(
            (opcode, SystemError::from_i32(errno)),
            (FUSE_LOOKUP, Some(SystemError::ENOENT))
                | (FUSE_FLUSH, Some(SystemError::ENOSYS))
                | (FUSE_GETXATTR, Some(SystemError::ENOSYS))
                | (FUSE_SETXATTR, Some(SystemError::ENOSYS))
                | (FUSE_LISTXATTR, Some(SystemError::ENOSYS))
                | (FUSE_REMOVEXATTR, Some(SystemError::ENOSYS))
                | (FUSE_INTERRUPT, Some(SystemError::EAGAIN_OR_EWOULDBLOCK))
        )
    }

    #[allow(dead_code)]
    pub fn negotiated_state(&self) -> (u32, u32, u32, u32, u16, u64) {
        let g = self.inner.lock();
        (
            g.init.minor,
            g.init.max_readahead,
            g.init.max_write,
            g.init.time_gran,
            g.init.max_pages,
            g.init.flags,
        )
    }

    pub fn max_write(&self) -> usize {
        let g = self.inner.lock();
        core::cmp::max(4096usize, g.init.max_write as usize)
    }

    pub fn max_read(&self) -> usize {
        let g = self.inner.lock();
        core::cmp::max(Self::MIN_MAX_WRITE, g.max_read as usize)
    }

    pub fn max_pages(&self) -> usize {
        let g = self.inner.lock();
        core::cmp::max(1, g.init.max_pages as usize)
    }

    pub fn max_readahead_pages(&self) -> usize {
        let g = self.inner.lock();
        let bytes = if g.init.max_readahead == 0 {
            Self::DEFAULT_MAX_READAHEAD
        } else {
            g.init.max_readahead as usize
        };
        core::cmp::max(1, bytes >> MMArch::PAGE_SHIFT)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::{mem::size_of, sync::atomic::Ordering};

    use system_error::SystemError;

    use super::stats;
    use super::{
        fuse_pack_struct, fuse_read_struct, FuseAttrOut, FuseConn, FuseEntryOut, FuseGetxattrIn,
        FuseGetxattrOut, FuseInHeader, FuseInitOut, FuseOpenOut, FuseOutHeader, FusePendingState,
        FuseReadIn, FuseReplyCapacitySource, FuseReplyContract, FuseRequestCred, FuseStatfsOut,
        FuseWriteOut, FUSE_ACCESS, FUSE_CREATE, FUSE_DESTROY, FUSE_FALLOCATE, FUSE_FLUSH,
        FUSE_FORGET, FUSE_FSYNC, FUSE_FSYNCDIR, FUSE_GETATTR, FUSE_GETXATTR, FUSE_INIT,
        FUSE_INTERRUPT, FUSE_LINK, FUSE_LISTXATTR, FUSE_LOOKUP, FUSE_MKDIR, FUSE_MKNOD, FUSE_OPEN,
        FUSE_OPENDIR, FUSE_READ, FUSE_READDIR, FUSE_READDIRPLUS, FUSE_READLINK, FUSE_RELEASE,
        FUSE_RELEASEDIR, FUSE_REMOVEXATTR, FUSE_RENAME, FUSE_RENAME2, FUSE_RMDIR, FUSE_SETATTR,
        FUSE_SETXATTR, FUSE_STATFS, FUSE_SYMLINK, FUSE_UNLINK, FUSE_WRITE,
    };

    fn set_minor(conn: &FuseConn, minor: u32) {
        conn.inner.lock().init.minor = minor;
        conn.reply_layout_minor.store(minor, Ordering::Release);
    }

    fn capacity(conn: &FuseConn, opcode: u32, payload: &[u8]) -> (usize, FuseReplyCapacitySource) {
        let capacity = conn.reply_capacity(opcode, payload).unwrap().unwrap();
        (capacity.bytes, capacity.source)
    }

    fn queue_test_request(conn: &Arc<FuseConn>, opcode: u32, pending_reply: bool) -> u64 {
        let unique = conn.alloc_unique();
        let req = conn
            .build_request(
                unique,
                opcode,
                1,
                &[],
                FuseRequestCred::nocreds(),
                !pending_reply,
            )
            .unwrap();
        let pending = pending_reply.then(|| Arc::new(FusePendingState::new(unique, opcode)));
        conn.push_request(req, pending, unique).unwrap();
        unique
    }

    fn read_opcode(buf: &[u8]) -> u32 {
        let hdr: FuseInHeader = fuse_read_struct(buf).unwrap();
        hdr.opcode
    }

    #[test]
    fn ordinary_read_does_not_consume_high_priority_queue() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        queue_test_request(&conn, FUSE_FORGET, false);
        queue_test_request(&conn, FUSE_LOOKUP, true);

        assert!(conn.has_pending_high_priority_requests());
        assert!(conn.has_pending_ordinary_requests());

        let mut buf = Vec::new();
        buf.resize(conn.min_read_buffer(), 0);
        let len = conn.read_ordinary_request(true, &mut buf).unwrap();
        assert_eq!(read_opcode(&buf[..len]), FUSE_LOOKUP);
        assert!(conn.has_pending_high_priority_requests());
        assert!(!conn.has_pending_ordinary_requests());

        let len = conn.read_high_priority_request(&mut buf).unwrap();
        assert_eq!(read_opcode(&buf[..len]), FUSE_FORGET);
        assert!(!conn.has_pending_high_priority_requests());
    }

    #[test]
    fn ordinary_read_returns_eagain_when_only_high_priority_is_pending() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        queue_test_request(&conn, FUSE_FORGET, false);

        let mut buf = Vec::new();
        buf.resize(conn.min_read_buffer(), 0);
        assert!(matches!(
            conn.read_ordinary_request(true, &mut buf),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        ));
        assert!(conn.has_pending_high_priority_requests());
    }

    #[test]
    fn virtiofs_direct_dequeue_transfers_existing_request() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        let unique = conn.alloc_unique();
        let req = conn
            .build_request(
                unique,
                FUSE_LOOKUP,
                1,
                b"name\0",
                FuseRequestCred::nocreds(),
                false,
            )
            .unwrap();
        let expected_ptr = req.bytes().as_ptr();
        conn.push_request(
            req,
            Some(Arc::new(FusePendingState::new(unique, FUSE_LOOKUP))),
            unique,
        )
        .unwrap();

        let dequeued = conn.dequeue_virtiofs_ordinary_request(8192).unwrap();
        assert_eq!(dequeued.opcode(), FUSE_LOOKUP);
        assert_eq!(dequeued.unique(), unique);
        assert_eq!(dequeued.bytes().as_ptr(), expected_ptr);
    }

    #[test]
    fn virtiofs_direct_dequeue_keeps_priority_queues_separate() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        queue_test_request(&conn, FUSE_FORGET, false);
        queue_test_request(&conn, FUSE_LOOKUP, true);

        let ordinary = conn.dequeue_virtiofs_ordinary_request(8192).unwrap();
        assert_eq!(ordinary.opcode(), FUSE_LOOKUP);
        let hiprio = conn.dequeue_virtiofs_high_priority_request(8192).unwrap();
        assert_eq!(hiprio.opcode(), FUSE_FORGET);
    }

    #[test]
    fn virtiofs_direct_dequeue_rejects_oversized_request() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        let unique = conn.alloc_unique();
        let req = conn
            .build_request(
                unique,
                FUSE_FORGET,
                1,
                &[0u8; 128],
                FuseRequestCred::nocreds(),
                true,
            )
            .unwrap();
        conn.push_request(req, None, unique).unwrap();
        queue_test_request(&conn, FUSE_FORGET, false);

        assert!(matches!(
            conn.dequeue_virtiofs_high_priority_request(64),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        ));
        assert!(conn.has_pending_high_priority_requests());
        let next = conn.dequeue_virtiofs_high_priority_request(8192).unwrap();
        assert_eq!(next.opcode(), FUSE_FORGET);
        assert!(!conn.has_pending_high_priority_requests());
    }

    #[test]
    fn normal_fuse_read_request_keeps_high_priority_visible() {
        let conn = FuseConn::new();
        queue_test_request(&conn, FUSE_FORGET, false);

        assert!(!conn.has_pending_high_priority_requests());
        assert!(conn.has_pending_ordinary_requests());

        let mut buf = Vec::new();
        buf.resize(conn.min_read_buffer(), 0);
        let len = conn.read_request(true, &mut buf).unwrap();
        assert_eq!(read_opcode(&buf[..len]), FUSE_FORGET);
    }

    #[test]
    fn bridge_wake_events_are_ignored_until_bridge_is_installed() {
        let conn = FuseConn::new();

        conn.wake_bridge(stats::VirtioFsBridgeWakeSource::Request);
        assert_eq!(conn.bridge_wake_events(), 0);

        conn.install_bridge_wake();
        conn.wake_bridge(stats::VirtioFsBridgeWakeSource::Request);
        assert_eq!(
            conn.bridge_wake_events(),
            stats::VirtioFsBridgeWakeSource::Request.bit()
        );
        assert_eq!(
            conn.take_bridge_wake_events(),
            stats::VirtioFsBridgeWakeSource::Request.bit()
        );

        conn.clear_bridge_wake();
        conn.wake_bridge(stats::VirtioFsBridgeWakeSource::Request);
        assert_eq!(conn.bridge_wake_events(), 0);
    }

    #[test]
    fn reply_capacity_covers_every_supported_opcode_without_fallback() {
        let conn = FuseConn::new_for_virtiofs(256 * 1024, 256 * 1024);
        set_minor(&conn, 39);
        let header = size_of::<FuseOutHeader>();
        let fixed = [
            (FUSE_LOOKUP, size_of::<FuseEntryOut>()),
            (FUSE_SYMLINK, size_of::<FuseEntryOut>()),
            (FUSE_MKNOD, size_of::<FuseEntryOut>()),
            (FUSE_MKDIR, size_of::<FuseEntryOut>()),
            (FUSE_LINK, size_of::<FuseEntryOut>()),
            (FUSE_GETATTR, size_of::<FuseAttrOut>()),
            (FUSE_SETATTR, size_of::<FuseAttrOut>()),
            (FUSE_OPEN, size_of::<FuseOpenOut>()),
            (FUSE_OPENDIR, size_of::<FuseOpenOut>()),
            (FUSE_WRITE, size_of::<FuseWriteOut>()),
            (FUSE_STATFS, size_of::<FuseStatfsOut>()),
            (
                FUSE_CREATE,
                size_of::<FuseEntryOut>() + size_of::<FuseOpenOut>(),
            ),
            (FUSE_INIT, size_of::<FuseInitOut>()),
        ];
        for (opcode, payload_len) in fixed {
            assert_eq!(
                capacity(&conn, opcode, &[]),
                (header + payload_len, FuseReplyCapacitySource::Exact)
            );
        }

        let header_only = [
            FUSE_UNLINK,
            FUSE_RMDIR,
            FUSE_RENAME,
            FUSE_RENAME2,
            FUSE_RELEASE,
            FUSE_FSYNC,
            FUSE_SETXATTR,
            FUSE_REMOVEXATTR,
            FUSE_FLUSH,
            FUSE_RELEASEDIR,
            FUSE_FSYNCDIR,
            FUSE_ACCESS,
            FUSE_INTERRUPT,
            FUSE_DESTROY,
            FUSE_FALLOCATE,
        ];
        for opcode in header_only {
            assert_eq!(
                capacity(&conn, opcode, &[]),
                (header, FuseReplyCapacitySource::Exact)
            );
        }

        let read_in = FuseReadIn {
            fh: 0,
            offset: 0,
            size: 4096,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        for opcode in [FUSE_READ, FUSE_READDIR, FUSE_READDIRPLUS] {
            assert_eq!(
                capacity(&conn, opcode, fuse_pack_struct(&read_in)),
                (header + 4096, FuseReplyCapacitySource::RequestBounded)
            );
        }
        assert_eq!(
            capacity(&conn, FUSE_READLINK, &[]),
            (
                header + <crate::arch::MMArch as crate::mm::MemoryManagementArch>::PAGE_SIZE - 1,
                FuseReplyCapacitySource::RequestBounded
            )
        );

        for opcode in [FUSE_GETXATTR, FUSE_LISTXATTR] {
            let query = FuseGetxattrIn {
                size: 0,
                padding: 0,
            };
            assert_eq!(
                capacity(&conn, opcode, fuse_pack_struct(&query)),
                (
                    header + size_of::<FuseGetxattrOut>(),
                    FuseReplyCapacitySource::RequestBounded
                )
            );
            let value = FuseGetxattrIn {
                size: 64 * 1024,
                padding: 0,
            };
            assert_eq!(
                capacity(&conn, opcode, fuse_pack_struct(&value)),
                (header + 64 * 1024, FuseReplyCapacitySource::RequestBounded)
            );
        }

        assert_eq!(
            capacity(&conn, 63, &[]),
            (256 * 1024, FuseReplyCapacitySource::ExplicitFallback)
        );
    }

    #[test]
    fn reply_contract_rejects_malformed_and_out_of_bounds_variable_requests() {
        let conn = FuseConn::new_for_virtiofs(256 * 1024, 256 * 1024);
        conn.configure_mount(0, 0, true, 4096);
        assert_eq!(
            conn.reply_capacity(FUSE_READ, &[]),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            conn.reply_capacity(FUSE_GETXATTR, &[]),
            Err(SystemError::EINVAL)
        );

        let too_large_read = FuseReadIn {
            fh: 0,
            offset: 0,
            size: 4097,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        assert_eq!(
            conn.reply_capacity(FUSE_READ, fuse_pack_struct(&too_large_read)),
            Err(SystemError::EINVAL)
        );
        let too_large_xattr = FuseGetxattrIn {
            size: 64 * 1024 + 1,
            padding: 0,
        };
        assert_eq!(
            conn.reply_capacity(FUSE_GETXATTR, fuse_pack_struct(&too_large_xattr)),
            Err(SystemError::EINVAL)
        );

        let undersized = FuseConn::new_for_virtiofs(8192, size_of::<FuseOutHeader>() - 1);
        assert_eq!(
            undersized.reply_capacity(63, &[]),
            Err(SystemError::EOVERFLOW)
        );
    }

    #[test]
    fn negotiated_minor_tightens_and_normalizes_compat_replies() {
        let conn = FuseConn::new_for_virtiofs(256 * 1024, 256 * 1024);
        let header = size_of::<FuseOutHeader>();
        set_minor(&conn, 3);
        assert_eq!(
            capacity(&conn, FUSE_STATFS, &[]).0,
            header + FuseConn::FUSE_COMPAT_STATFS_SIZE
        );
        assert_eq!(
            capacity(&conn, FUSE_LOOKUP, &[]).0,
            header + FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE
        );
        assert_eq!(
            capacity(&conn, FUSE_GETATTR, &[]).0,
            header + FuseConn::FUSE_COMPAT_ATTR_OUT_SIZE
        );
        assert_eq!(
            capacity(&conn, FUSE_CREATE, &[]).0,
            header + FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE + size_of::<FuseOpenOut>()
        );

        let compat_entry = vec![0x5a; FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE];
        let normalized = FuseConn::normalize_compat_reply(3, FUSE_LOOKUP, &compat_entry).unwrap();
        assert_eq!(normalized.len(), size_of::<FuseEntryOut>());
        assert_eq!(
            &normalized[..FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE],
            &compat_entry
        );
        assert!(normalized[FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE..]
            .iter()
            .all(|byte| *byte == 0));

        let compat_create_len = FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE + size_of::<FuseOpenOut>();
        let compat_create = vec![0xa5; compat_create_len];
        let normalized = FuseConn::normalize_compat_reply(3, FUSE_CREATE, &compat_create).unwrap();
        assert_eq!(
            &normalized[size_of::<FuseEntryOut>()..],
            &compat_create[FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE..]
        );
    }

    #[test]
    fn init_reply_accepts_zero_extended_compat_prefix_only() {
        assert!(matches!(
            FuseConn::parse_init_reply(&[0u8; 7]),
            Err(SystemError::EINVAL)
        ));
        let mut compat = [0u8; FuseConn::FUSE_COMPAT_INIT_OUT_SIZE];
        compat[..4].copy_from_slice(&super::FUSE_KERNEL_VERSION.to_ne_bytes());
        compat[4..].copy_from_slice(&3u32.to_ne_bytes());
        let parsed = FuseConn::parse_init_reply(&compat).unwrap();
        assert_eq!(parsed.major, super::FUSE_KERNEL_VERSION);
        assert_eq!(parsed.minor, 3);
        assert_eq!(parsed.max_write, 0);
        assert!(matches!(
            FuseConn::parse_init_reply(&vec![0u8; size_of::<FuseInitOut>() + 1]),
            Err(SystemError::EINVAL)
        ));
    }

    #[test]
    fn forget_is_no_reply_while_interrupt_and_destroy_have_header_capacity() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        let unique = conn.alloc_unique();
        let forget = conn
            .build_request(
                unique,
                FUSE_FORGET,
                1,
                &[],
                FuseRequestCred::nocreds(),
                true,
            )
            .unwrap();
        assert_eq!(forget.reply_contract(), FuseReplyContract::NoReply);

        for opcode in [FUSE_INTERRUPT, FUSE_DESTROY] {
            let req = conn
                .build_request(
                    conn.alloc_unique(),
                    opcode,
                    0,
                    &[],
                    FuseRequestCred::nocreds(),
                    false,
                )
                .unwrap();
            assert!(matches!(
                req.reply_contract(),
                FuseReplyContract::Reply {
                    capacity: Some(capacity)
                } if capacity.bytes == size_of::<FuseOutHeader>()
            ));
        }
    }

    #[test]
    fn zero_length_destroy_completion_finishes_unique_and_disconnects() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        let unique = conn.alloc_unique();
        let pending = Arc::new(FusePendingState::new(unique, FUSE_DESTROY));
        let req = conn
            .build_request(
                unique,
                FUSE_DESTROY,
                0,
                &[],
                FuseRequestCred::nocreds(),
                false,
            )
            .unwrap();
        conn.push_request(req, Some(pending.clone()), unique)
            .unwrap();
        conn.dequeue_virtiofs_ordinary_request(8192).unwrap();

        conn.complete_destroy_without_reply(unique).unwrap();
        assert!(!conn.is_connected());
        assert_eq!(pending.wait_complete().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn teardown_gate_is_idempotent_and_rejects_late_business_requests() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        {
            let mut g = conn.inner.lock();
            g.initialized = true;
            g.mounted = true;
        }

        conn.on_umount();
        conn.on_umount();
        {
            let g = conn.inner.lock();
            assert!(g.teardown_started);
            assert_eq!(
                g.pending
                    .iter()
                    .filter(|req| req.opcode == FUSE_DESTROY)
                    .count(),
                1
            );
        }

        let unique = conn.alloc_unique();
        let request = conn
            .build_request(
                unique,
                FUSE_LOOKUP,
                1,
                b"late\0",
                FuseRequestCred::nocreds(),
                false,
            )
            .unwrap();
        let pending = Arc::new(FusePendingState::new(unique, FUSE_LOOKUP));
        assert_eq!(
            conn.push_request(request, Some(pending), unique),
            Err(SystemError::ENOTCONN)
        );
    }
}
