use alloc::{collections::BTreeMap, collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::{
    filesystem::epoll::{
        event_poll::EventPoll, event_poll::LockedEPItemLinkedList, EPollEventType, EPollItem,
    },
    libs::{
        mutex::Mutex,
        wait_queue::{WaitQueue, Waiter},
    },
    process::ProcessManager,
};

use super::protocol::{
    fuse_pack_struct, fuse_read_struct, FuseInHeader, FuseInitIn, FuseInitOut, FuseOutHeader,
    FUSE_INIT, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION,
};

#[derive(Debug)]
pub struct FuseRequest {
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct FusePendingState {
    opcode: u32,
    response: Mutex<Option<Result<Vec<u8>, SystemError>>>,
    wait: WaitQueue,
}

impl FusePendingState {
    pub fn new(opcode: u32) -> Self {
        Self {
            opcode,
            response: Mutex::new(None),
            wait: WaitQueue::default(),
        }
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
        // Avoid TOCTOU between response update and wait queue registration.
        if let Some(res) = self.response.lock().take() {
            return res;
        }
        loop {
            let mut guard = self.response.lock();
            if let Some(res) = guard.take() {
                return res;
            }

            let (waiter, waker) = Waiter::new_pair();
            self.wait.register_waker(waker.clone())?;

            // Re-check under the same lock after registering.
            if let Some(res) = guard.take() {
                self.wait.remove_waker(&waker);
                return res;
            }
            drop(guard);

            if let Err(e) = waiter.wait(true) {
                self.wait.remove_waker(&waker);
                return Err(e);
            }
        }
    }
}

#[derive(Debug)]
struct FuseConnInner {
    connected: bool,
    mounted: bool,
    initialized: bool,
    pending: VecDeque<Arc<FuseRequest>>,
    processing: BTreeMap<u64, Arc<FusePendingState>>,
}

/// FUSE connection object (roughly equivalent to Linux `struct fuse_conn`).
#[derive(Debug)]
pub struct FuseConn {
    inner: Mutex<FuseConnInner>,
    next_unique: AtomicU64,
    read_wait: WaitQueue,
    init_wait: WaitQueue,
    epitems: LockedEPItemLinkedList,
}

impl FuseConn {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FuseConnInner {
                connected: true,
                mounted: false,
                initialized: false,
                pending: VecDeque::new(),
                processing: BTreeMap::new(),
            }),
            // Use non-zero unique, keep even IDs for "ordinary" requests as Linux does.
            next_unique: AtomicU64::new(2),
            read_wait: WaitQueue::default(),
            init_wait: WaitQueue::default(),
            epitems: LockedEPItemLinkedList::default(),
        })
    }

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

    pub fn is_initialized(&self) -> bool {
        self.inner.lock().initialized
    }

    fn alloc_unique(&self) -> u64 {
        self.next_unique.fetch_add(2, Ordering::Relaxed)
    }

    fn wait_initialized(&self) -> Result<(), SystemError> {
        if self.is_initialized() {
            return Ok(());
        }
        // Bind condition checks to inner lock and register waker before releasing it.
        loop {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if g.initialized {
                return Ok(());
            }

            let (waiter, waker) = Waiter::new_pair();
            self.init_wait.register_waker(waker.clone())?;

            if !g.connected {
                self.init_wait.remove_waker(&waker);
                return Err(SystemError::ENOTCONN);
            }
            if g.initialized {
                self.init_wait.remove_waker(&waker);
                return Ok(());
            }
            drop(g);

            if let Err(e) = waiter.wait(true) {
                self.init_wait.remove_waker(&waker);
                return Err(e);
            }
        }
    }

    pub fn abort(&self) {
        let processing: Vec<Arc<FusePendingState>> = {
            let mut g = self.inner.lock();
            g.connected = false;
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

    pub fn read_request(&self, nonblock: bool, out: &mut [u8]) -> Result<usize, SystemError> {
        // Linux: if O_NONBLOCK, return EAGAIN.

        let req = if nonblock {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            g.pending
                .pop_front()
                .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?
        } else {
            loop {
                let mut g = self.inner.lock();
                if !g.connected {
                    return Err(SystemError::ENOTCONN);
                }
                if let Some(req) = g.pending.pop_front() {
                    break req;
                }

                let (waiter, waker) = Waiter::new_pair();
                self.read_wait.register_waker(waker.clone())?;

                if !g.connected {
                    self.read_wait.remove_waker(&waker);
                    return Err(SystemError::ENOTCONN);
                }
                if let Some(req) = g.pending.pop_front() {
                    self.read_wait.remove_waker(&waker);
                    break req;
                }
                drop(g);

                if let Err(e) = waiter.wait(true) {
                    self.read_wait.remove_waker(&waker);
                    return Err(e);
                }
            }
        };

        if out.len() < req.bytes.len() {
            // Put it back and report EINVAL: user must provide a sufficiently large buffer.
            let mut g = self.inner.lock();
            if g.connected {
                g.pending.push_front(req);
            }
            return Err(SystemError::EINVAL);
        }

        out[..req.bytes.len()].copy_from_slice(&req.bytes);
        Ok(req.bytes.len())
    }

    pub fn enqueue_init(&self) -> Result<(), SystemError> {
        let init_in = FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            flags2: 0,
            unused: [0; 11],
        };
        self.enqueue_request(FUSE_INIT, 0, fuse_pack_struct(&init_in))
            .map(|_| ())
    }

    pub fn request(&self, opcode: u32, nodeid: u64, payload: &[u8]) -> Result<Vec<u8>, SystemError> {
        if opcode != FUSE_INIT {
            self.wait_initialized()?;
        }
        self.enqueue_request(opcode, nodeid, payload)?.wait_complete()
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
        let pid = pcb
            .task_tgid_vnr()
            .map(|p| p.data() as u32)
            .unwrap_or(0);

        let hdr = FuseInHeader {
            len: (core::mem::size_of::<FuseInHeader>() + payload.len()) as u32,
            opcode,
            unique,
            nodeid,
            uid: cred.fsuid.data() as u32,
            gid: cred.fsgid.data() as u32,
            pid,
            total_extlen: 0,
            padding: 0,
        };

        let mut bytes = Vec::with_capacity(hdr.len as usize);
        bytes.extend_from_slice(fuse_pack_struct(&hdr));
        bytes.extend_from_slice(payload);

        let req = Arc::new(FuseRequest { bytes });
        let pending_state = Arc::new(FusePendingState::new(opcode));

        {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            g.pending.push_back(req);
            g.processing.insert(unique, pending_state.clone());
        }

        self.read_wait.wakeup(None);
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
        Ok(pending_state)
    }

    pub fn write_reply(&self, data: &[u8]) -> Result<usize, SystemError> {
        if data.len() < core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }

        let out_hdr: FuseOutHeader = fuse_read_struct(data)?;
        if out_hdr.len as usize != data.len() {
            return Err(SystemError::EINVAL);
        }

        let pending = {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            g.processing.remove(&out_hdr.unique)
                .ok_or(SystemError::EINVAL)?
        };

        let payload = &data[core::mem::size_of::<FuseOutHeader>()..];
        let error = out_hdr.error;

        if error != 0 {
            // Negative errno from userspace.
            let errno = -error;
            let e = SystemError::from_i32(errno).unwrap_or(SystemError::EIO);
            pending.complete(Err(e));
            if pending.opcode == FUSE_INIT {
                self.abort();
            }
            return Ok(data.len());
        }

        if pending.opcode == FUSE_INIT {
            let init_out: FuseInitOut = fuse_read_struct(payload)?;
            if init_out.major != FUSE_KERNEL_VERSION {
                pending.complete(Err(SystemError::EINVAL));
                self.abort();
                return Ok(data.len());
            }

            // Negotiate minor version: use the smaller one.
            let _negotiated_minor = core::cmp::min(init_out.minor, FUSE_KERNEL_MINOR_VERSION);

            {
                let mut g = self.inner.lock();
                if g.connected {
                    g.initialized = true;
                }
            }
            self.init_wait.wakeup(None);
        }

        pending.complete(Ok(payload.to_vec()));
        Ok(data.len())
    }
}
