use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use system_error::SystemError;

use crate::{
    filesystem::epoll::{
        event_poll::EventPoll, event_poll::LockedEPItemLinkedList, EPollEventType, EPollItem,
    },
    libs::{mutex::Mutex, wait_queue::WaitQueue},
};

use super::protocol::{
    fuse_pack_struct, fuse_read_struct, FuseInHeader, FuseInitIn, FuseInitOut, FuseOutHeader,
    FUSE_INIT, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseReqKind {
    Init,
}

#[derive(Debug)]
pub struct FuseRequest {
    pub unique: u64,
    pub kind: FuseReqKind,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
struct FuseConnInner {
    connected: bool,
    mounted: bool,
    initialized: bool,
    pending: VecDeque<Arc<FuseRequest>>,
    processing: BTreeMap<u64, FuseReqKind>,
}

/// FUSE connection object (roughly equivalent to Linux `struct fuse_conn`).
#[derive(Debug)]
pub struct FuseConn {
    inner: Mutex<FuseConnInner>,
    next_unique: AtomicU64,
    read_wait: WaitQueue,
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

    pub fn enqueue_init(&self) -> Result<(), SystemError> {
        let unique = self.alloc_unique();

        let hdr = FuseInHeader {
            len: (core::mem::size_of::<FuseInHeader>() + core::mem::size_of::<FuseInitIn>()) as u32,
            opcode: FUSE_INIT,
            unique,
            nodeid: 0,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

        let init_in = FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            flags2: 0,
            unused: [0; 11],
        };

        let mut bytes = Vec::with_capacity(hdr.len as usize);
        bytes.extend_from_slice(fuse_pack_struct(&hdr));
        bytes.extend_from_slice(fuse_pack_struct(&init_in));

        let req = Arc::new(FuseRequest {
            unique,
            kind: FuseReqKind::Init,
            bytes,
        });

        {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            g.pending.push_back(req.clone());
            g.processing.insert(unique, FuseReqKind::Init);
        }

        self.read_wait.wakeup(None);
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
        Ok(())
    }

    pub fn abort(&self) {
        {
            let mut g = self.inner.lock();
            g.connected = false;
        }
        self.read_wait.wakeup(None);
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
            self.read_wait.wait_until_interruptible(|| {
                let mut g = self.inner.lock();
                if !g.connected {
                    return Some(Err(SystemError::ENOTCONN));
                }
                g.pending.pop_front().map(Ok)
            })??
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

    pub fn write_reply(&self, data: &[u8]) -> Result<usize, SystemError> {
        if data.len() < core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }

        let out_hdr: FuseOutHeader = fuse_read_struct(data)?;
        if out_hdr.len as usize != data.len() {
            return Err(SystemError::EINVAL);
        }

        let kind = {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            match g.processing.remove(&out_hdr.unique) {
                Some(k) => k,
                None => return Err(SystemError::EINVAL),
            }
        };

        if out_hdr.error != 0 {
            // Any init-stage error aborts the connection.
            self.abort();
            return Err(SystemError::EINVAL);
        }

        match kind {
            FuseReqKind::Init => {
                let payload = &data[core::mem::size_of::<FuseOutHeader>()..];
                let init_out: FuseInitOut = fuse_read_struct(payload)?;
                if init_out.major != FUSE_KERNEL_VERSION {
                    self.abort();
                    return Err(SystemError::EINVAL);
                }

                // Negotiate minor version: use the smaller one.
                let negotiated_minor = core::cmp::min(init_out.minor, FUSE_KERNEL_MINOR_VERSION);

                let mut g = self.inner.lock();
                if !g.connected {
                    return Err(SystemError::ENOTCONN);
                }
                g.initialized = true;
                drop(g);

                // For now we only record "initialized"; other negotiated values can be added later.
                let _ = negotiated_minor;
            }
        }

        Ok(data.len())
    }
}
