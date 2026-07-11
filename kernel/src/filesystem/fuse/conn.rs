use alloc::{
    collections::BTreeMap,
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use system_error::SystemError;

use crate::filesystem::page_cache::PageCacheReadDmaReservation;
use crate::{
    arch::MMArch,
    filesystem::epoll::{
        event_poll::EventPoll, event_poll::LockedEPItemLinkedList, EPollEventType,
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
    FuseInHeader, FuseWriteIn, FUSE_ABORT_ERROR, FUSE_ASYNC_DIO, FUSE_ASYNC_READ,
    FUSE_ATOMIC_O_TRUNC, FUSE_AUTO_INVAL_DATA, FUSE_BIG_WRITES, FUSE_DESTROY, FUSE_DONT_MASK,
    FUSE_DO_READDIRPLUS, FUSE_EXPLICIT_INVAL_DATA, FUSE_EXPORT_SUPPORT, FUSE_FORGET, FUSE_FSYNC,
    FUSE_FSYNCDIR, FUSE_GETXATTR, FUSE_HANDLE_KILLPRIV, FUSE_INIT_EXT, FUSE_INTERRUPT,
    FUSE_LISTXATTR, FUSE_MAX_PAGES, FUSE_NO_OPENDIR_SUPPORT, FUSE_NO_OPEN_SUPPORT,
    FUSE_PARALLEL_DIROPS, FUSE_POSIX_ACL, FUSE_POSIX_LOCKS, FUSE_READDIRPLUS_AUTO,
    FUSE_REMOVEXATTR, FUSE_SETXATTR, FUSE_SUBMOUNTS,
};
use super::reply::{FuseReadPagesReply, FuseReply};
use super::{stats, trace};

mod daemon;
mod request;
pub(crate) use request::BackgroundReadPagesCtx;

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
    read_pages_destination: Option<Arc<PageCacheReadDmaReservation>>,
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
    pub(crate) retained_bytes: usize,
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

    pub(crate) fn read_pages_destination(&self) -> Option<&Arc<PageCacheReadDmaReservation>> {
        self.read_pages_destination.as_ref()
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
    response: Mutex<PendingCompletion>,
    wait: WaitQueue,
    background_credit: Mutex<Option<FuseBackgroundCredit>>,
    read_completion: Option<FuseReadCompletion>,
}

#[derive(Debug)]
struct FuseReadCompletion {
    target: Arc<PageCacheReadDmaReservation>,
    node: Weak<super::inode::FuseNode>,
    start_page: usize,
    requested: usize,
    observed_size: usize,
    observed_attr_version: u64,
    open_pin: Mutex<Option<super::private_data::FuseOpenLifetimePin>>,
}

impl FuseReadCompletion {
    fn release_open_pin(&self) {
        self.open_pin.lock().take();
    }

    fn finish(&self, result: &Result<FusePendingResult, SystemError>) -> Result<(), SystemError> {
        let node = self.node.upgrade();
        if let Some(node) = &node {
            let start = self.start_page.saturating_mul(MMArch::PAGE_SIZE);
            if node.attr_version() != self.observed_attr_version
                && node
                    .cached_metadata_snapshot()
                    .is_some_and(|metadata| metadata.size.max(0) as usize <= start)
            {
                let _ = self.target.rollback(SystemError::EIO);
                return Ok(());
            }
        }
        let bytes = match result {
            Ok(FusePendingResult::Reply(reply)) => {
                if reply.len() > self.requested {
                    self.target.rollback(SystemError::EIO)?;
                    return Err(SystemError::EIO);
                }
                let bytes = reply.len();
                self.target.publish_contiguous(reply)?;
                bytes
            }
            Ok(FusePendingResult::ReadPagesDirect { bytes }) => {
                if *bytes > self.requested {
                    self.target.rollback(SystemError::EIO)?;
                    return Err(SystemError::EIO);
                }
                self.target.publish_completed(*bytes)?;
                *bytes
            }
            Err(error) => {
                let _ = self.target.rollback(error.clone());
                return Ok(());
            }
        };
        if bytes < self.requested {
            stats::on_readahead_short_read();
            if let Some(node) = node {
                let (eof, _) = node.note_short_read_eof(
                    self.start_page,
                    bytes,
                    self.observed_size,
                    self.observed_attr_version,
                )?;
                node.discard_completed_pages_beyond(&self.target, eof);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FuseBackgroundInner {
    connected: bool,
    max: usize,
    congestion: usize,
    inflight: usize,
}

#[derive(Debug)]
struct FuseBackgroundState {
    inner: Mutex<FuseBackgroundInner>,
    wait: WaitQueue,
}

#[derive(Debug)]
struct FuseBackgroundCredit {
    state: Arc<FuseBackgroundState>,
}

impl Drop for FuseBackgroundCredit {
    fn drop(&mut self) {
        let mut inner = self.state.inner.lock();
        inner.inflight = inner.inflight.saturating_sub(1);
        drop(inner);
        stats::on_background_released();
        self.state.wait.wakeup(None);
    }
}

impl FuseBackgroundState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FuseBackgroundInner {
                connected: true,
                max: FuseConn::DEFAULT_MAX_BACKGROUND,
                congestion: FuseConn::DEFAULT_CONGESTION_THRESHOLD,
                inflight: 0,
            }),
            wait: WaitQueue::default(),
        })
    }

    fn configure(&self, max: usize, congestion: usize) {
        let mut inner = self.inner.lock();
        inner.max = core::cmp::max(1, max);
        inner.congestion = core::cmp::min(core::cmp::max(1, congestion), inner.max);
        drop(inner);
        self.wait.wakeup(None);
    }

    fn disconnect(&self) {
        self.inner.lock().connected = false;
        self.wait.wakeup(None);
    }

    fn acquire(
        self: &Arc<Self>,
        speculative: bool,
    ) -> Result<Option<FuseBackgroundCredit>, SystemError> {
        wait_with_recheck(&self.wait, || {
            let mut inner = self.inner.lock();
            if !inner.connected {
                return Err(SystemError::ENOTCONN);
            }
            let limit = if speculative {
                inner.congestion
            } else {
                inner.max
            };
            if inner.inflight < limit {
                inner.inflight += 1;
                stats::on_background_acquired();
                return Ok(Some(Some(FuseBackgroundCredit {
                    state: self.clone(),
                })));
            }
            if speculative {
                stats::on_background_pressure(true);
                return Ok(Some(None));
            }
            stats::on_background_pressure(false);
            Ok(None)
        })
    }
}

#[derive(Debug)]
enum PendingCompletion {
    Waiting,
    Completing,
    Ready(Result<FusePendingResult, SystemError>),
    Consumed,
}

#[derive(Debug)]
pub(crate) enum FusePendingResult {
    Reply(FuseReply),
    ReadPagesDirect { bytes: usize },
}

enum FuseReadWaitOutcome {
    Complete(Result<FuseReadPagesReply, SystemError>),
    Interrupted,
}

impl FusePendingState {
    pub fn new(unique: u64, opcode: u32) -> Self {
        Self::new_with_credit(unique, opcode, None, None)
    }

    fn new_with_credit(
        unique: u64,
        opcode: u32,
        background_credit: Option<FuseBackgroundCredit>,
        read_completion: Option<FuseReadCompletion>,
    ) -> Self {
        Self {
            unique,
            opcode,
            response: Mutex::new(PendingCompletion::Waiting),
            wait: WaitQueue::default(),
            background_credit: Mutex::new(background_credit),
            read_completion,
        }
    }

    pub fn unique(&self) -> u64 {
        self.unique
    }

    pub fn complete(&self, v: Result<FuseReply, SystemError>) -> bool {
        self.complete_result(v.map(FusePendingResult::Reply))
    }

    /// Complete a page-cache read whose payload was written into its owned
    /// destination.  This deliberately shares the ordinary pending state so
    /// duplicate replies, disconnect and teardown have one retirement point.
    pub(crate) fn complete_read_pages_direct(&self, bytes: usize) -> bool {
        self.complete_result(Ok(FusePendingResult::ReadPagesDirect { bytes }))
    }

    fn complete_result(&self, mut v: Result<FusePendingResult, SystemError>) -> bool {
        let mut guard = self.response.lock();
        if !matches!(*guard, PendingCompletion::Waiting) {
            // Duplicate replies are ignored (Linux does similarly).
            return false;
        }
        *guard = PendingCompletion::Completing;
        drop(guard);
        if let Some(completion) = &self.read_completion {
            if let Err(error) = completion.finish(&v) {
                v = Err(error);
            }
            completion.release_open_pin();
        }
        let mut guard = self.response.lock();
        *guard = PendingCompletion::Ready(v);
        drop(guard);
        // Linux releases a background slot at request completion, not when a
        // waiter later consumes the result.  Taking the token makes this
        // exactly-once across replies, abort and teardown.
        self.background_credit.lock().take();
        self.wait.wakeup(None);
        true
    }

    pub fn wait_complete(&self) -> Result<FuseReply, SystemError> {
        match self.wait_result()? {
            FusePendingResult::Reply(reply) => Ok(reply),
            // A direct completion is a request-contract violation for ordinary
            // callers.  Do not expose an empty reply that could look valid.
            FusePendingResult::ReadPagesDirect { .. } => Err(SystemError::EIO),
        }
    }

    #[cfg(test)]
    pub(crate) fn wait_read_pages_complete(&self) -> Result<FuseReadPagesReply, SystemError> {
        match self.wait_result()? {
            FusePendingResult::Reply(reply) => Ok(FuseReadPagesReply::Contiguous(reply)),
            FusePendingResult::ReadPagesDirect { bytes } => {
                Ok(FuseReadPagesReply::Direct { bytes })
            }
        }
    }

    fn wait_read_pages_once(&self) -> FuseReadWaitOutcome {
        let take_ready = || {
            let mut guard = self.response.lock();
            if matches!(*guard, PendingCompletion::Ready(_)) {
                let ready = core::mem::replace(&mut *guard, PendingCompletion::Consumed);
                if let PendingCompletion::Ready(result) = ready {
                    return Some(result.map(|result| match result {
                        FusePendingResult::Reply(reply) => FuseReadPagesReply::Contiguous(reply),
                        FusePendingResult::ReadPagesDirect { bytes } => {
                            FuseReadPagesReply::Direct { bytes }
                        }
                    }));
                }
            }
            None
        };
        if let Some(result) = take_ready() {
            return FuseReadWaitOutcome::Complete(result);
        }
        loop {
            let (waiter, waker) = Waiter::new_pair();
            if let Err(error) = self.wait.register_waker(waker.clone()) {
                return FuseReadWaitOutcome::Complete(Err(error));
            }
            if let Some(result) = take_ready() {
                self.wait.remove_waker(&waker);
                return FuseReadWaitOutcome::Complete(result);
            }
            if waiter.wait(true).is_err() {
                self.wait.remove_waker(&waker);
                return FuseReadWaitOutcome::Interrupted;
            }
        }
    }

    fn wait_result(&self) -> Result<FusePendingResult, SystemError> {
        wait_with_recheck(&self.wait, || {
            let mut guard = self.response.lock();
            if matches!(*guard, PendingCompletion::Ready(_)) {
                let ready = core::mem::replace(&mut *guard, PendingCompletion::Consumed);
                if let PendingCompletion::Ready(res) = ready {
                    return Ok(Some(res));
                }
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
    background: Arc<FuseBackgroundState>,
}

impl FuseConn {
    const FUSE_INT_REQ_BIT: u64 = 1;
    // Keep this in sync with `sys_read.rs` userspace chunking size.
    const USER_READ_CHUNK: usize = 64 * 1024;
    const MIN_MAX_WRITE: usize = 4096;
    const DEFAULT_MAX_PAGES: usize = 32;
    const MAX_MAX_PAGES: usize = 256;
    const DEFAULT_MAX_READAHEAD: usize = 128 * MMArch::PAGE_SIZE;
    const DEFAULT_MAX_BACKGROUND: usize = 12;
    const DEFAULT_CONGESTION_THRESHOLD: usize = 9;
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
            background: FuseBackgroundState::new(),
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
        self.background.disconnect();
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
        self.background.disconnect();
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

    fn acquire_background_credit(
        &self,
        speculative: bool,
    ) -> Result<Option<FuseBackgroundCredit>, SystemError> {
        {
            let inner = self.inner.lock();
            if !inner.connected || inner.teardown_started {
                return Err(SystemError::ENOTCONN);
            }
        }
        self.background.acquire(speculative)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::{mem::size_of, sync::atomic::Ordering};

    use system_error::SystemError;

    use super::super::protocol::{
        FuseEntryOut, FuseOpenOut, FuseOutHeader, FuseStatfsOut, FUSE_CREATE, FUSE_DESTROY,
        FUSE_GETATTR, FUSE_LOOKUP, FUSE_STATFS,
    };
    use super::{daemon, request, stats, FuseConn, FusePendingState, FuseReplyCapacitySource};

    fn set_minor(conn: &FuseConn, minor: u32) {
        conn.inner.lock().init.minor = minor;
        conn.reply_layout_minor.store(minor, Ordering::Release);
    }

    fn capacity(conn: &FuseConn, opcode: u32, payload: &[u8]) -> (usize, FuseReplyCapacitySource) {
        let capacity = request::reply_capacity_for_test(conn, opcode, payload)
            .unwrap()
            .unwrap();
        (capacity.bytes, capacity.source)
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
    fn negotiated_minor_tightens_and_normalizes_compat_replies() {
        let conn = FuseConn::new_for_virtiofs(256 * 1024, 256 * 1024);
        let header = size_of::<FuseOutHeader>();
        set_minor(&conn, 3);
        let statfs_capacity = request::reply_capacity_for_test(&conn, FUSE_STATFS, &[])
            .unwrap()
            .unwrap();
        assert_eq!(
            statfs_capacity.bytes,
            header + FuseConn::FUSE_COMPAT_STATFS_SIZE
        );
        assert_eq!(statfs_capacity.retained_bytes, size_of::<FuseStatfsOut>());
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
        let normalized =
            daemon::normalize_compat_reply_for_test(3, FUSE_LOOKUP, &compat_entry).unwrap();
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
        let normalized =
            daemon::normalize_compat_reply_for_test(3, FUSE_CREATE, &compat_create).unwrap();
        assert_eq!(
            &normalized[size_of::<FuseEntryOut>()..],
            &compat_create[FuseConn::FUSE_COMPAT_ENTRY_OUT_SIZE..]
        );
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

        assert_eq!(
            conn.request_nocreds(FUSE_LOOKUP, 1, b"late\0"),
            Err(SystemError::ENOTCONN)
        );
    }

    #[test]
    fn pending_read_pages_preserves_direct_and_contiguous_results() {
        let direct = FusePendingState::new(1, super::super::protocol::FUSE_READ);
        assert!(direct.complete_read_pages_direct(4097));
        assert!(!direct.complete_read_pages_direct(1));
        assert!(matches!(
            direct.wait_read_pages_complete().unwrap(),
            super::super::reply::FuseReadPagesReply::Direct { bytes: 4097 }
        ));

        let contiguous = FusePendingState::new(3, super::super::protocol::FUSE_READ);
        assert!(contiguous.complete(Ok(super::super::reply::FuseReply::from_bytes(vec![1, 2]))));
        assert!(matches!(
            contiguous.wait_read_pages_complete().unwrap(),
            super::super::reply::FuseReadPagesReply::Contiguous(reply) if &*reply == [1, 2]
        ));
    }

    #[test]
    fn ordinary_wait_rejects_direct_completion() {
        let pending = FusePendingState::new(1, super::super::protocol::FUSE_READ);
        assert!(pending.complete_read_pages_direct(1));
        assert_eq!(pending.wait_complete(), Err(SystemError::EIO));
    }
}
