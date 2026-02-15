use alloc::{collections::BTreeMap, collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::{
    arch::MMArch,
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
    fuse_pack_struct, fuse_read_struct, FuseForgetIn, FuseInHeader, FuseInitIn, FuseInitOut,
    FuseInterruptIn, FuseOutHeader, FuseWriteIn, FUSE_ABORT_ERROR, FUSE_ASYNC_DIO, FUSE_ASYNC_READ,
    FUSE_ATOMIC_O_TRUNC, FUSE_AUTO_INVAL_DATA, FUSE_BIG_WRITES, FUSE_DESTROY, FUSE_DONT_MASK,
    FUSE_DO_READDIRPLUS, FUSE_EXPLICIT_INVAL_DATA, FUSE_EXPORT_SUPPORT, FUSE_FLUSH, FUSE_FORGET,
    FUSE_HANDLE_KILLPRIV, FUSE_INIT, FUSE_INIT_EXT, FUSE_INTERRUPT, FUSE_KERNEL_MINOR_VERSION,
    FUSE_KERNEL_VERSION, FUSE_LOOKUP, FUSE_MAX_PAGES, FUSE_MIN_READ_BUFFER, FUSE_NOTIFY_DELETE,
    FUSE_NOTIFY_INVAL_ENTRY, FUSE_NOTIFY_INVAL_INODE, FUSE_NOTIFY_POLL, FUSE_NOTIFY_RETRIEVE,
    FUSE_NOTIFY_STORE, FUSE_NO_OPENDIR_SUPPORT, FUSE_NO_OPEN_SUPPORT, FUSE_PARALLEL_DIROPS,
    FUSE_POSIX_ACL, FUSE_POSIX_LOCKS, FUSE_READDIRPLUS_AUTO, FUSE_WRITEBACK_CACHE,
};

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
pub struct FuseRequest {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct FuseRequestCred {
    uid: u32,
    gid: u32,
    pid: u32,
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
            max_pages: 1,
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
    init: FuseInitNegotiated,
    no_open: bool,
    no_opendir: bool,
    no_readdirplus: bool,
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
    epitems: LockedEPItemLinkedList,
}

impl FuseConn {
    // Keep this in sync with `sys_read.rs` userspace chunking size.
    const USER_READ_CHUNK: usize = 64 * 1024;

    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FuseConnInner {
                connected: true,
                mounted: false,
                initialized: false,
                owner_uid: 0,
                owner_gid: 0,
                allow_other: false,
                init: FuseInitNegotiated::default(),
                no_open: false,
                no_opendir: false,
                no_readdirplus: false,
                pending: VecDeque::new(),
                processing: BTreeMap::new(),
            }),
            // Use non-zero unique, keep even IDs for "ordinary" requests as Linux does.
            next_unique: AtomicU64::new(2),
            dev_count: AtomicUsize::new(1),
            read_wait: WaitQueue::default(),
            init_wait: WaitQueue::default(),
            epitems: LockedEPItemLinkedList::default(),
        })
    }

    #[allow(dead_code)]
    pub fn is_mounted(&self) -> bool {
        self.inner.lock().mounted
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

    pub fn configure_mount(&self, owner_uid: u32, owner_gid: u32, allow_other: bool) {
        let mut g = self.inner.lock();
        g.owner_uid = owner_uid;
        g.owner_gid = owner_gid;
        g.allow_other = allow_other;
    }

    fn has_init_flag(&self, flag: u64) -> bool {
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
        let processing: Vec<Arc<FusePendingState>> = {
            let mut g = self.inner.lock();
            g.connected = false;
            g.mounted = false;
            g.pending.clear();
            let processing = g.processing.values().cloned().collect();
            g.processing.clear();
            processing
        };
        for p in processing {
            p.complete(Err(SystemError::ENOTCONN));
        }
        self.read_wait.wakeup(None);
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
        let should_destroy: bool;
        {
            let mut g = self.inner.lock();
            should_destroy = g.connected && g.initialized;
            g.mounted = false;
            g.pending.clear();
            processing = g.processing.values().cloned().collect();
            g.processing.clear();
        }

        for p in processing {
            p.complete(Err(SystemError::ENOTCONN));
        }
        self.init_wait.wakeup(None);

        if !should_destroy {
            self.abort();
            return;
        }

        if self.enqueue_noreply(FUSE_DESTROY, 0, &[]).is_err() {
            self.abort();
            return;
        }
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
            g.connected && g.mounted && g.initialized
        };
        if !can_send {
            return Ok(());
        }
        let inarg = FuseInterruptIn { unique };
        let _ = self.enqueue_request(FUSE_INTERRUPT, 0, fuse_pack_struct(&inarg))?;
        Ok(())
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
        let have_pending = !g.pending.is_empty();
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
            4096
        } else {
            Self::USER_READ_CHUNK - overhead
        }
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
        g.pending
            .pop_front()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn pop_pending_blocking(&self) -> Result<Arc<FuseRequest>, SystemError> {
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

    pub fn read_request(&self, nonblock: bool, out: &mut [u8]) -> Result<usize, SystemError> {
        // Linux: require a sane minimum read buffer for all reads.
        let min_read = self.min_read_buffer();
        if out.len() < min_read {
            log::warn!(
                "fuse: read buffer too small: got={} min={} nonblock={}",
                out.len(),
                min_read,
                nonblock
            );
            return Err(SystemError::EINVAL);
        }

        // Linux: if O_NONBLOCK and no pending request, return EAGAIN.
        let req = if nonblock {
            self.pop_pending_nonblock()?
        } else {
            self.pop_pending_blocking()?
        };

        if out.len() < req.bytes.len() {
            // Put it back and report EINVAL: userspace must provide a sufficiently large buffer.
            let req_len = req.bytes.len();
            let mut g = self.inner.lock();
            if g.connected {
                g.pending.push_front(req);
            }
            log::warn!(
                "fuse: read buffer smaller than queued request: got={} need={}",
                out.len(),
                req_len
            );
            return Err(SystemError::EINVAL);
        }

        out[..req.bytes.len()].copy_from_slice(&req.bytes);
        Ok(req.bytes.len())
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
            | FUSE_WRITEBACK_CACHE
            | FUSE_NO_OPEN_SUPPORT
            | FUSE_PARALLEL_DIROPS
            | FUSE_HANDLE_KILLPRIV
            | FUSE_POSIX_ACL
            | FUSE_ABORT_ERROR
            | FUSE_MAX_PAGES
            | FUSE_NO_OPENDIR_SUPPORT
            | FUSE_EXPLICIT_INVAL_DATA
            | FUSE_INIT_EXT
    }

    pub fn enqueue_init(&self) -> Result<(), SystemError> {
        let flags = Self::kernel_init_flags();
        let init_in = FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
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
        );
        self.push_request(req, None, unique)?;
        Ok(())
    }

    fn enqueue_request(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<Arc<FusePendingState>, SystemError> {
        let unique = self.alloc_unique();

        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        if !self.allow_current_process(&cred) {
            return Err(SystemError::EACCES);
        }
        let pid = pcb.task_tgid_vnr().map(|p| p.data() as u32).unwrap_or(0);
        let pending_state = Arc::new(FusePendingState::new(unique, opcode));
        let req = self.build_request(
            unique,
            opcode,
            nodeid,
            payload,
            FuseRequestCred {
                uid: cred.fsuid.data() as u32,
                gid: cred.fsgid.data() as u32,
                pid,
            },
        );
        self.push_request(req, Some(pending_state.clone()), unique)?;
        Ok(pending_state)
    }

    fn build_request(
        &self,
        unique: u64,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
        req_cred: FuseRequestCred,
    ) -> Arc<FuseRequest> {
        let hdr = FuseInHeader {
            len: (core::mem::size_of::<FuseInHeader>() + payload.len()) as u32,
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
        Arc::new(FuseRequest { bytes })
    }

    fn push_request(
        &self,
        req: Arc<FuseRequest>,
        pending_state: Option<Arc<FusePendingState>>,
        unique: u64,
    ) -> Result<(), SystemError> {
        {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            g.pending.push_back(req);
            if let Some(pending) = pending_state {
                g.processing.insert(unique, pending);
            }
        }

        self.read_wait.wakeup(None);
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
        Ok(())
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

        let pending = {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOENT);
            }
            g.processing
                .remove(&out_hdr.unique)
                .ok_or(SystemError::ENOENT)?
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
            if pending.opcode == FUSE_INIT {
                self.abort();
            }
            return Ok(data.len());
        }

        if pending.opcode == FUSE_INIT {
            let init_out: FuseInitOut = match fuse_read_struct(payload) {
                Ok(v) => v,
                Err(e) => {
                    pending.complete(Err(e));
                    self.abort();
                    return Ok(data.len());
                }
            };

            if init_out.major != FUSE_KERNEL_VERSION {
                pending.complete(Err(SystemError::EINVAL));
                self.abort();
                return Ok(data.len());
            }

            let mut negotiated_flags = init_out.flags as u64;
            if (negotiated_flags & FUSE_INIT_EXT) != 0 {
                negotiated_flags |= (init_out.flags2 as u64) << 32;
            }
            let negotiated_minor = core::cmp::min(init_out.minor, FUSE_KERNEL_MINOR_VERSION);
            let negotiated_max_pages_raw =
                if (negotiated_flags & FUSE_MAX_PAGES) != 0 && init_out.max_pages != 0 {
                    init_out.max_pages
                } else {
                    1
                };
            let negotiated_max_write = core::cmp::max(4096usize, init_out.max_write as usize);
            let max_write_cap = Self::max_write_cap_for_user_read_chunk();
            let capped_max_write = core::cmp::min(negotiated_max_write, max_write_cap);
            if capped_max_write < negotiated_max_write {
                log::trace!(
                    "fuse: cap negotiated max_write from {} to {} due user read chunk limit",
                    negotiated_max_write,
                    capped_max_write
                );
            }
            let pages_from_write =
                core::cmp::max(1usize, capped_max_write.div_ceil(MMArch::PAGE_SIZE)) as u16;
            let negotiated_max_pages = core::cmp::min(negotiated_max_pages_raw, pages_from_write);

            {
                let mut g = self.inner.lock();
                if g.connected {
                    g.initialized = true;
                    g.init = FuseInitNegotiated {
                        minor: negotiated_minor,
                        max_readahead: init_out.max_readahead,
                        max_write: capped_max_write as u32,
                        time_gran: init_out.time_gran,
                        max_pages: negotiated_max_pages,
                        flags: negotiated_flags,
                    };
                }
            }
            self.init_wait.wakeup(None);
        }

        pending.complete(Ok(payload.to_vec()));
        Ok(data.len())
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
}
