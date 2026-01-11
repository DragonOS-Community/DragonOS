use crate::{
    filesystem::epoll::{event_poll::EventPoll, EPollEventType},
    filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId},
    libs::mutex::Mutex,
    libs::rwsem::RwSem,
    net::socket::{self, *},
};
use crate::{
    libs::wait_queue::WaitQueue,
    net::{
        posix::MsgHdr,
        socket::{
            common::EPollItems,
            endpoint::Endpoint,
            unix::{
                stream::inner::{get_backlog, Backlog},
                UnixEndpoint,
            },
        },
    },
};
use alloc::sync::{Arc, Weak};
use core::num::Wrapping;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use inner::{Connected, Init, Inner};
use system_error::SystemError;

use crate::filesystem::vfs::iov::IoVecs;
use crate::net::socket::unix::{current_ucred, nobody_ucred, UCred};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;
use crate::syscall::user_access::UserBufferReader;
use crate::time::{Duration, Instant};

// Use common ancillary message types from parent module
use super::{cmsg_align, CmsgBuffer, Cmsghdr, MSG_CTRUNC, SCM_CREDENTIALS, SCM_RIGHTS, SOL_SOCKET};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Linger {
    l_onoff: i32,
    l_linger: i32,
}

impl Linger {
    const SIZE: usize = core::mem::size_of::<Linger>();

    fn from_optval(optval: &[u8]) -> Result<Self, SystemError> {
        if optval.len() < Self::SIZE {
            return Err(SystemError::EINVAL);
        }
        let mut on = [0u8; 4];
        let mut linger = [0u8; 4];
        on.copy_from_slice(&optval[..4]);
        linger.copy_from_slice(&optval[4..8]);
        let l_onoff = i32::from_ne_bytes(on);
        let l_linger = i32::from_ne_bytes(linger);
        if l_linger < 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(Self { l_onoff, l_linger })
    }

    fn write_to(&self, out: &mut [u8]) -> Result<usize, SystemError> {
        if out.len() < Self::SIZE {
            return Err(SystemError::EINVAL);
        }
        out[..4].copy_from_slice(&self.l_onoff.to_ne_bytes());
        out[4..8].copy_from_slice(&self.l_linger.to_ne_bytes());
        Ok(Self::SIZE)
    }
}

pub mod inner;

fn ring_cap_for_effective_sockbuf(effective: usize) -> usize {
    // Underlying ring buffer requires power-of-two capacity.
    let mut cap = effective.next_power_of_two();
    cap = core::cmp::max(cap, inner::UNIX_STREAM_DEFAULT_BUF_SIZE);
    cap = core::cmp::min(cap, UnixStreamSocket::MAX_EFFECTIVE_SOCKET_BUF_SIZE);
    cap
}

#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct UnixStreamSocket {
    inner: RwSem<Option<Inner>>,
    //todo options
    epitems: EPollItems,
    fasync_items: FAsyncItems,
    wait_queue: Arc<WaitQueue>,
    inode_id: InodeId,
    open_files: AtomicUsize,
    netns: Arc<NetNamespace>,
    self_weak: Weak<UnixStreamSocket>,
    /// Peer socket for socket pairs (used for SIGIO notification)
    peer: Mutex<Option<Weak<UnixStreamSocket>>>,

    is_nonblocking: AtomicBool,
    is_seqpacket: bool,

    passcred: AtomicBool,

    linger: Mutex<Linger>,

    /// Pending connection-reset to be reported on the next read/recv.
    /// Linux behavior: when a unix stream socket is closed with unread data in its
    /// receive queue, the peer observes ECONNRESET (af_unix.c:unix_release_sock).
    connreset_pending: AtomicBool,

    sndbuf: AtomicUsize,
    rcvbuf: AtomicUsize,
    send_timeout_us: AtomicU64,
    recv_timeout_us: AtomicU64,
}

impl UnixStreamSocket {
    /// 默认的元数据缓冲区大小
    #[allow(dead_code)]
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    pub const MIN_SOCKET_BUF_SIZE: usize = 1024;
    /// Upper bound for the *effective* (doubled) SO_SNDBUF/SO_RCVBUF value.
    /// Must be a power-of-two friendly size to keep ring buffer resizes sane.
    const MAX_EFFECTIVE_SOCKET_BUF_SIZE: usize = 8 * 1024 * 1024;

    pub(super) fn new_init(
        init: Init,
        is_nonblocking: bool,
        is_seqpacket: bool,
        netns: Arc<NetNamespace>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|self_weak| Self {
            inner: RwSem::new(Some(Inner::Init(init))),
            wait_queue: Arc::new(WaitQueue::default()),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            netns,
            self_weak: self_weak.clone(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            peer: Mutex::new(None),
            passcred: AtomicBool::new(false),

            linger: Mutex::new(Linger::default()),
            connreset_pending: AtomicBool::new(false),

            sndbuf: AtomicUsize::new(inner::UNIX_STREAM_DEFAULT_BUF_SIZE),
            rcvbuf: AtomicUsize::new(inner::UNIX_STREAM_DEFAULT_BUF_SIZE),
            send_timeout_us: AtomicU64::new(0),
            recv_timeout_us: AtomicU64::new(0),
        })
    }

    pub(super) fn new_connected(
        connected: Connected,
        is_nonblocking: bool,
        is_seqpacket: bool,
        netns: Arc<NetNamespace>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|self_weak| Self {
            inner: RwSem::new(Some(Inner::Connected(connected))),
            wait_queue: Arc::new(WaitQueue::default()),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            netns,
            self_weak: self_weak.clone(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            peer: Mutex::new(None),
            passcred: AtomicBool::new(false),

            linger: Mutex::new(Linger::default()),
            connreset_pending: AtomicBool::new(false),

            sndbuf: AtomicUsize::new(inner::UNIX_STREAM_DEFAULT_BUF_SIZE),
            rcvbuf: AtomicUsize::new(inner::UNIX_STREAM_DEFAULT_BUF_SIZE),
            send_timeout_us: AtomicU64::new(0),
            recv_timeout_us: AtomicU64::new(0),
        })
    }

    fn parse_i32_opt(optval: &[u8]) -> Result<i32, SystemError> {
        if optval.len() < 4 {
            return Err(SystemError::EINVAL);
        }
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&optval[..4]);
        Ok(i32::from_ne_bytes(raw))
    }

    fn parse_timeval_opt(optval: &[u8]) -> Result<Duration, SystemError> {
        // Accept both 64-bit and 32-bit timeval layouts.
        if optval.len() >= 16 {
            let mut sec_raw = [0u8; 8];
            let mut usec_raw = [0u8; 8];
            sec_raw.copy_from_slice(&optval[..8]);
            usec_raw.copy_from_slice(&optval[8..16]);
            let sec = i64::from_ne_bytes(sec_raw);
            let usec = i64::from_ne_bytes(usec_raw);
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(SystemError::EINVAL);
            }
            let total_us = (sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(usec as u64);
            return Ok(Duration::from_micros(total_us));
        }

        if optval.len() >= 12 {
            let mut sec_raw = [0u8; 8];
            let mut usec_raw = [0u8; 4];
            sec_raw.copy_from_slice(&optval[..8]);
            usec_raw.copy_from_slice(&optval[8..12]);
            let sec = i64::from_ne_bytes(sec_raw);
            let usec = i32::from_ne_bytes(usec_raw) as i64;
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(SystemError::EINVAL);
            }
            let total_us = (sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(usec as u64);
            return Ok(Duration::from_micros(total_us));
        }

        Err(SystemError::EINVAL)
    }

    fn write_timeval(value: &mut [u8], us: u64) -> Result<usize, SystemError> {
        if value.len() < 16 {
            return Err(SystemError::EINVAL);
        }
        let sec = (us / 1_000_000) as i64;
        let usec = (us % 1_000_000) as i64;
        value[..8].copy_from_slice(&sec.to_ne_bytes());
        value[8..16].copy_from_slice(&usec.to_ne_bytes());
        Ok(16)
    }

    fn effective_sockbuf(requested: usize) -> usize {
        let requested = core::cmp::max(Self::MIN_SOCKET_BUF_SIZE, requested);
        let effective = requested.saturating_mul(2);
        core::cmp::min(effective, Self::MAX_EFFECTIVE_SOCKET_BUF_SIZE)
    }

    fn send_timeout(&self) -> Option<Duration> {
        let us = self.send_timeout_us.load(Ordering::Relaxed);
        if us == 0 {
            None
        } else {
            Some(Duration::from_micros(us))
        }
    }

    fn recv_timeout(&self) -> Option<Duration> {
        let us = self.recv_timeout_us.load(Ordering::Relaxed);
        if us == 0 {
            None
        } else {
            Some(Duration::from_micros(us))
        }
    }

    fn wake_peer_writable(&self) {
        if let Some(peer_weak) = self.peer.lock().as_ref() {
            if let Some(peer) = peer_weak.upgrade() {
                peer.wait_queue
                    .wakeup(Some(crate::process::ProcessState::Blocked(true)));
                let _ = EventPoll::wakeup_epoll(
                    peer.epoll_items().as_ref(),
                    EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM,
                );
                peer.fasync_items.send_sigio();
            }
        }
    }

    pub fn new(is_nonblocking: bool, is_seqpacket: bool) -> Arc<Self> {
        let netns = ProcessManager::current_netns();
        Self::new_init(Init::new(), is_nonblocking, is_seqpacket, netns)
    }

    pub fn new_pair(is_nonblocking: bool, is_seqpacket: bool) -> (Arc<Self>, Arc<Self>) {
        let (conn_a, conn_b) = Connected::new_pair(None, None);
        let netns = ProcessManager::current_netns();
        let socket_a = Self::new_connected(conn_a, is_nonblocking, is_seqpacket, netns.clone());
        let socket_b = Self::new_connected(conn_b, is_nonblocking, is_seqpacket, netns);

        // Set up peer references for SIGIO notification
        *socket_a.peer.lock() = Some(Arc::downgrade(&socket_b));
        *socket_b.peer.lock() = Some(Arc::downgrade(&socket_a));

        (socket_a, socket_b)
    }

    pub fn ioctl_fionread(&self) -> usize {
        match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected.readable_len(self.is_seqpacket),
            _ => 0,
        }
    }

    pub fn ioctl_tiocoutq(&self) -> usize {
        match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected.outq_len(self.is_seqpacket),
            _ => 0,
        }
    }

    fn try_send_with_meta(
        &self,
        buffer: &[u8],
    ) -> Result<(usize, Wrapping<usize>, usize), SystemError> {
        match self.inner.read().as_ref().expect("inner is None") {
            Inner::Connected(connected) => {
                let sndbuf = self.sndbuf.load(Ordering::Relaxed);
                connected.try_send(buffer, self.is_seqpacket, sndbuf)
            }
            _ => {
                // log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN);
            }
        }
    }

    fn try_recv(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        match self.inner.read().as_ref().expect("inner is None") {
            Inner::Connected(connected) => connected.try_recv(buffer, self.is_seqpacket),
            _ => {
                log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN);
            }
        }
    }

    fn try_connect(&self, backlog: &Arc<Backlog>) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("inner is None");

        let (inner, result) = match inner {
            Inner::Init(init) => {
                let this = self
                    .self_weak
                    .upgrade()
                    .expect("UnixStreamSocket self_weak upgrade failed");
                let snd = self.sndbuf.load(Ordering::Relaxed);
                let rcv = self.rcvbuf.load(Ordering::Relaxed);
                match backlog.push_incoming(init, this, self.is_seqpacket, snd, rcv) {
                    Ok(connected) => (Inner::Connected(connected), Ok(())),
                    Err((init, err)) => (Inner::Init(init), Err(err)),
                }
            }
            Inner::Listener(inner) => (Inner::Listener(inner), Err(SystemError::EINVAL)),
            Inner::Connected(connected) => (Inner::Connected(connected), Err(SystemError::EISCONN)),
        };

        match result {
            Ok(()) | Err(SystemError::EINPROGRESS) => {}
            _ => {}
        }

        writer.replace(inner);

        result
    }

    pub fn try_accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        match self.inner.write().as_mut().expect("inner is None") {
            Inner::Listener(listener) => {
                listener.try_accept(self.passcred.load(core::sync::atomic::Ordering::Relaxed)) as _
            }
            _ => {
                log::error!("the socket is not listening");
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn is_nonblocking(&self) -> bool {
        self.is_nonblocking
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    fn can_recv(&self) -> bool {
        match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => {
                connected.readable_len(self.is_seqpacket) != 0 || connected.peer_send_shutdown()
            }
            _ => false,
        }
    }

    fn is_acceptable(&self) -> bool {
        match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Listener(listener) => listener.is_acceptable(),
            _ => false,
        }
    }

    fn take_connreset_from_peer(&self) -> bool {
        match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected.take_connreset_from_peer(),
            _ => false,
        }
    }

    fn send_with_timeout(
        &self,
        buffer: &[u8],
        flags: socket::PMSG,
    ) -> Result<(usize, Wrapping<usize>, usize), SystemError> {
        // Linux: zero-length send on stream sockets succeeds immediately.
        if !self.is_seqpacket && buffer.is_empty() {
            return Ok((0, Wrapping(0), 0));
        }

        let nonblock = self.is_nonblocking() || flags.contains(socket::PMSG::DONTWAIT);

        if self.is_seqpacket {
            // For SOCK_SEQPACKET, reject messages larger than SO_SNDBUF.
            // Account for our internal 4-byte record header.
            let needed = buffer.len().saturating_add(core::mem::size_of::<u32>());
            if needed > self.sndbuf.load(Ordering::Relaxed) {
                return Err(SystemError::EMSGSIZE);
            }
        }

        let deadline = self.send_timeout().map(|t| Instant::now() + t);
        loop {
            match self.try_send_with_meta(buffer) {
                Ok(v) => return Ok(v),
                Err(SystemError::ENOBUFS) if nonblock => {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                }
                Err(SystemError::ENOBUFS) => {
                    let timeout = deadline
                        .map(|d| d.duration_since(Instant::now()).unwrap_or(Duration::ZERO));
                    self.wait_queue.wait_event_interruptible_timeout(
                        || match self
                            .inner
                            .read()
                            .as_ref()
                            .expect("UnixStreamSocket inner is None")
                        {
                            Inner::Connected(connected) => {
                                // Must match Connected::try_send(): it requires the *entire*
                                // payload (plus seqpacket header) to fit.
                                let need = if self.is_seqpacket {
                                    buffer.len() + core::mem::size_of::<u32>()
                                } else {
                                    buffer.len()
                                };

                                let sndbuf = self.sndbuf.load(Ordering::Relaxed);
                                let queued = connected.outq_len(self.is_seqpacket);
                                connected.send_free_len() >= need
                                    && queued.saturating_add(need) <= sndbuf
                            }
                            _ => true,
                        },
                        timeout,
                    )?;

                    if let Some(d) = deadline {
                        if Instant::now() >= d {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Socket for UnixStreamSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn recv_bytes_available(&self) -> usize {
        self.ioctl_fionread()
    }

    fn send_bytes_available(&self) -> Result<usize, SystemError> {
        Ok(self.ioctl_tiocoutq())
    }

    fn set_nonblocking(&self, nonblocking: bool) {
        self.is_nonblocking
            .store(nonblocking, core::sync::atomic::Ordering::Relaxed);
    }

    fn connect(&self, server_endpoint: Endpoint) -> Result<(), SystemError> {
        let remote_addr = UnixEndpoint::try_from(server_endpoint)?.connect_in(&self.netns)?;
        let backlog = get_backlog(&remote_addr)?;

        if self.is_nonblocking() {
            self.try_connect(&backlog)
        } else {
            backlog.pause_until(|| self.try_connect(&backlog))
        }
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let addr = UnixEndpoint::try_from(endpoint)?;

        let mut writer = self.inner.write();
        match writer.as_mut().expect("UnixStreamSocket inner is None") {
            Inner::Init(init) => init.bind(addr, &self.netns),
            Inner::Connected(connected) => connected.bind(addr, &self.netns),
            Inner::Listener(_listener) => addr.bind_unnamed(),
        }
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        const SOMAXCONN: usize = 4096;
        let backlog = backlog.min(SOMAXCONN);

        let mut writer = self.inner.write();

        let (inner, err) = match writer.take().expect("UnixStreamSocket inner is None") {
            Inner::Init(init) => {
                let snd = self.sndbuf.load(Ordering::Relaxed);
                let rcv = self.rcvbuf.load(Ordering::Relaxed);
                match init.listen(
                    backlog,
                    self.is_seqpacket,
                    self.wait_queue.clone(),
                    snd,
                    rcv,
                    self.netns.clone(),
                ) {
                    Ok(listener) => (Inner::Listener(listener), None),
                    Err((err, init)) => (Inner::Init(init), Some(err)),
                }
            }
            Inner::Listener(listener) => {
                listener.listen(backlog);
                (Inner::Listener(listener), None)
            }
            Inner::Connected(connected) => (Inner::Connected(connected), Some(SystemError::EINVAL)),
        };

        writer.replace(inner);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }

        return Ok(());
    }

    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        // debug!("stream server begin accept");
        if self.is_nonblocking() {
            self.try_accept()
        } else {
            loop {
                match self.try_accept() {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.is_acceptable(), {})?
                    }
                    result => break result,
                }
            }
        }
    }

    fn set_option(&self, level: PSOL, optname: usize, optval: &[u8]) -> Result<(), SystemError> {
        if !matches!(level, PSOL::SOCKET) {
            return Err(SystemError::ENOPROTOOPT);
        }

        let opt = crate::net::socket::PSO::try_from(optname as u32)
            .map_err(|_| SystemError::ENOPROTOOPT)?;
        match opt {
            crate::net::socket::PSO::SNDBUF | crate::net::socket::PSO::SNDBUFFORCE => {
                let requested = Self::parse_i32_opt(optval)?;
                if requested < 0 {
                    return Err(SystemError::EINVAL);
                }

                let effective = Self::effective_sockbuf(requested as usize);
                self.sndbuf.store(effective, Ordering::SeqCst);

                let new_cap = ring_cap_for_effective_sockbuf(effective);
                if let Some(inner) = self.inner.read().as_ref() {
                    match inner {
                        Inner::Connected(connected) => {
                            let cur = connected.send_capacity();
                            if new_cap > cur {
                                connected.resize_sendbuf(new_cap)?;
                            }
                        }
                        Inner::Listener(listener) => {
                            listener.set_sndbuf_effective(effective);
                        }
                        Inner::Init(_) => {}
                    }
                }
                Ok(())
            }
            crate::net::socket::PSO::RCVBUF | crate::net::socket::PSO::RCVBUFFORCE => {
                let requested = Self::parse_i32_opt(optval)?;
                if requested < 0 {
                    return Err(SystemError::EINVAL);
                }

                let effective = Self::effective_sockbuf(requested as usize);
                self.rcvbuf.store(effective, Ordering::SeqCst);

                let new_cap = ring_cap_for_effective_sockbuf(effective);
                if let Some(inner) = self.inner.read().as_ref() {
                    match inner {
                        Inner::Connected(connected) => {
                            let cur = connected.recv_capacity();
                            if new_cap > cur {
                                connected.resize_recvbuf(new_cap)?;
                            }
                        }
                        Inner::Listener(listener) => {
                            listener.set_rcvbuf_effective(effective);
                        }
                        Inner::Init(_) => {}
                    }
                }
                Ok(())
            }
            crate::net::socket::PSO::LINGER => {
                let linger = Linger::from_optval(optval)?;
                *self.linger.lock() = linger;
                Ok(())
            }
            crate::net::socket::PSO::SNDTIMEO_OLD | crate::net::socket::PSO::SNDTIMEO_NEW => {
                let d = Self::parse_timeval_opt(optval)?;
                self.send_timeout_us
                    .store(d.total_micros(), Ordering::SeqCst);
                Ok(())
            }
            crate::net::socket::PSO::RCVTIMEO_OLD | crate::net::socket::PSO::RCVTIMEO_NEW => {
                let d = Self::parse_timeval_opt(optval)?;
                self.recv_timeout_us
                    .store(d.total_micros(), Ordering::SeqCst);
                Ok(())
            }
            crate::net::socket::PSO::PASSCRED => {
                if optval.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let mut v = [0u8; 4];
                v.copy_from_slice(&optval[..4]);
                let on = i32::from_ne_bytes(v) != 0;
                self.passcred
                    .store(on, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn wait_queue(&self) -> &WaitQueue {
        return &self.wait_queue;
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let addr: Endpoint = match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Init(init) => init
                .endpoint()
                .unwrap_or(Endpoint::Unix(UnixEndpoint::Unnamed)),
            Inner::Connected(connected) => connected.endpoint(),
            Inner::Listener(listener) => listener.endpoint(),
        };

        Ok(addr)
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        let peer_addr = match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected.peer_endpoint(),
            _ => return Err(SystemError::ENOTCONN),
        };

        Ok(peer_addr.into())
    }

    fn recv(&self, buffer: &mut [u8], _flags: socket::PMSG) -> Result<usize, SystemError> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let nonblock = self.is_nonblocking() || _flags.contains(socket::PMSG::DONTWAIT);
        loop {
            let result = if self.is_seqpacket {
                let peek = _flags.contains(socket::PMSG::PEEK);
                match self.inner.read().as_ref().expect("inner is None") {
                    Inner::Connected(connected) => connected
                        .try_recv_seqpacket_meta(buffer, peek)
                        .map(|(copy_len, orig_len, _truncated)| {
                            if _flags.contains(socket::PMSG::TRUNC) {
                                orig_len
                            } else {
                                copy_len
                            }
                        }),
                    _ => Err(SystemError::ENOTCONN),
                }
            } else if _flags.contains(socket::PMSG::PEEK) {
                match self.inner.read().as_ref().expect("inner is None") {
                    Inner::Connected(connected) => connected.try_peek(buffer, self.is_seqpacket),
                    _ => Err(SystemError::ENOTCONN),
                }
            } else {
                self.try_recv(buffer)
            };

            match result {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) if !nonblock => {
                    self.wait_queue.wait_event_interruptible_timeout(
                        || self.can_recv(),
                        self.recv_timeout(),
                    )?;
                    continue;
                }
                Ok(n) => {
                    if n == 0 && !_flags.contains(socket::PMSG::PEEK) {
                        // Prefer ring-buffer based reset signaling (works for all connection types).
                        // Keep the per-socket fallback for socketpair legacy paths.
                        let ring_reset = self.take_connreset_from_peer();
                        let sock_reset = self
                            .connreset_pending
                            .swap(false, core::sync::atomic::Ordering::SeqCst);
                        if ring_reset || sock_reset {
                            return Err(SystemError::ECONNRESET);
                        }
                    }
                    if n != 0 && !_flags.contains(socket::PMSG::PEEK) {
                        self.wake_peer_writable();
                    }
                    return Ok(n);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        _flags: socket::PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // 对于流式 Unix Socket，recv_from 与 recv 类似
        // 直接调用 try_recv 并返回对端地址
        let recv_len = self.recv(buffer, _flags)?;

        // 获取对端地址
        let peer_endpoint = match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected.peer_endpoint(),
            _ => return Err(SystemError::ENOTCONN),
        };

        Ok((recv_len, peer_endpoint.into()))

        // if flags.contains(PMSG::OOB) {
        //     return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        // }
        // if !flags.contains(PMSG::DONTWAIT) {
        //     loop {
        //         log::debug!("socket try recv from");

        //         wq_wait_event_interruptible!(
        //             self.wait_queue,
        //             self.can_recv()? || self.is_peer_shutdown()?,
        //             {}
        //         )?;
        //         // connect锁和flag判断顺序不正确，应该先判断在
        //         log::debug!("try recv");

        //         match &*self.inner.write() {
        //             Inner::Connected(connected) => match connected.try_recv(buffer) {
        //                 Ok(usize) => {
        //                     log::debug!("recvs from successfully");
        //                     return Ok((usize, connected.peer_endpoint().unwrap().clone()));
        //                 }
        //                 Err(_) => continue,
        //             },
        //             _ => {
        //                 log::error!("the socket is not connected");
        //                 return Err(SystemError::ENOTCONN);
        //             }
        //         }
        //     }
        // } else {
        //     unimplemented!("unimplemented non_block")
        // }
    }

    fn recv_msg(&self, _msg: &mut MsgHdr, _flags: socket::PMSG) -> Result<usize, SystemError> {
        let msg = _msg;

        let control_ptr = msg.msg_control;
        let control_len = msg.msg_controllen;

        // Scatter destination is described by msg_iov/msg_iovlen in user memory.
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let mut buf = iovs.new_buf(true);

        // Read payload first.
        let nonblock = self.is_nonblocking() || _flags.contains(socket::PMSG::DONTWAIT);
        let peek = _flags.contains(socket::PMSG::PEEK);
        let want_creds = self.passcred.load(core::sync::atomic::Ordering::Relaxed);

        let (payload_copy_len, orig_len, truncated, ret_len, scm_cred, scm_rights) = if self
            .is_seqpacket
        {
            loop {
                match self
                    .inner
                    .read()
                    .as_ref()
                    .expect("UnixStreamSocket inner is None")
                {
                    Inner::Connected(connected) => {
                        // IMPORTANT: snapshot SCM before consuming bytes.
                        // try_recv_seqpacket_meta() consumes from the ring buffer
                        // using pop_slice(), which discards record metadata.
                        let snapshot = connected.scm_snapshot_for_recvmsg();
                        match connected.try_recv_seqpacket_meta(&mut buf[..], peek) {
                            Ok((copy_len, orig_len, truncated)) => {
                                let ret_len = if _flags.contains(socket::PMSG::TRUNC) {
                                    orig_len
                                } else {
                                    copy_len
                                };
                                let (scm_cred, scm_rights) =
                                    snapshot.scm_data.unwrap_or((None, alloc::vec::Vec::new()));
                                break (
                                    copy_len, orig_len, truncated, ret_len, scm_cred, scm_rights,
                                );
                            }
                            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) if !nonblock => {
                                self.wait_queue.wait_event_interruptible_timeout(
                                    || self.can_recv(),
                                    self.recv_timeout(),
                                )?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    _ => return Err(SystemError::ENOTCONN),
                }
            }
        } else {
            loop {
                match self
                    .inner
                    .read()
                    .as_ref()
                    .expect("UnixStreamSocket inner is None")
                {
                    Inner::Connected(connected) => {
                        match connected.try_recv_stream_recvmsg_meta(&mut buf, peek, want_creds) {
                            Ok(meta) => {
                                let n = meta.copy_len;
                                break (n, n, false, n, meta.scm_cred, meta.scm_rights);
                            }
                            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) if !nonblock => {
                                self.wait_queue.wait_event_interruptible_timeout(
                                    || self.can_recv(),
                                    self.recv_timeout(),
                                )?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    _ => return Err(SystemError::ENOTCONN),
                }
            }
        };

        if payload_copy_len != 0 {
            iovs.scatter(&buf[..payload_copy_len])?;
            if !peek {
                self.wake_peer_writable();
            }
        }

        // Default: no flags.
        msg.msg_flags = 0;
        if truncated {
            msg.msg_flags |= socket::PMSG::TRUNC.bits() as i32;
        }
        // Default: no control returned.
        msg.msg_controllen = 0;

        // EOF (or empty record): no ancillary data.
        if orig_len == 0 && payload_copy_len == 0 {
            return Ok(0);
        }

        let (scm_cred, scm_rights) = (scm_cred, scm_rights);
        let has_rights = !scm_rights.is_empty();
        if !want_creds && !has_rights {
            // No ancillary to return.
            return Ok(ret_len);
        }

        // If userspace didn't provide a control buffer, just report truncation.
        if control_ptr.is_null() || control_len == 0 {
            msg.msg_flags |= MSG_CTRUNC;
            msg.msg_controllen = 0;
            return Ok(ret_len);
        }

        let hdr_len = core::mem::size_of::<Cmsghdr>();
        let mut write_off = 0usize;

        // 1) SCM_CREDENTIALS (if enabled)
        if want_creds {
            let remaining = control_len.saturating_sub(write_off);
            let data_avail = remaining.saturating_sub(cmsg_align(hdr_len));
            let full_data_len = core::mem::size_of::<UCred>();
            let cred_copy_len = core::cmp::min(data_avail, full_data_len);

            let cred_to_send = scm_cred.unwrap_or_else(nobody_ucred);
            let cred_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(
                    (&cred_to_send as *const UCred) as *const u8,
                    full_data_len,
                )
            };
            let mut buf = CmsgBuffer {
                ptr: control_ptr,
                len: control_len,
                write_off: &mut write_off,
            };
            buf.put(
                &mut msg.msg_flags,
                SOL_SOCKET,
                SCM_CREDENTIALS,
                full_data_len,
                &cred_bytes[..cred_copy_len],
            )?;
        }

        // 2) SCM_RIGHTS
        if has_rights {
            let remaining = control_len - write_off;
            let data_avail = remaining.saturating_sub(cmsg_align(hdr_len));
            let max_fds = data_avail / core::mem::size_of::<i32>();
            let fit = core::cmp::min(max_fds, scm_rights.len());
            if fit == 0 {
                msg.msg_flags |= MSG_CTRUNC;
                msg.msg_controllen = write_off;
                return Ok(ret_len);
            }

            if fit < scm_rights.len() {
                msg.msg_flags |= MSG_CTRUNC;
            }

            let cloexec = _flags.contains(socket::PMSG::CMSG_CLOEXEC);
            let mut received_fds: alloc::vec::Vec<i32> = alloc::vec::Vec::with_capacity(fit);
            {
                let fd_table_binding = ProcessManager::current_pcb().fd_table();
                let mut fd_table = fd_table_binding.write();
                for file in scm_rights.iter().take(fit) {
                    let new_file = file.as_ref().try_clone().ok_or(SystemError::EINVAL)?;
                    new_file.set_close_on_exec(cloexec);
                    let new_fd = fd_table.alloc_fd(new_file, None)?;
                    received_fds.push(new_fd);
                }
            }

            let data_len = received_fds.len() * core::mem::size_of::<i32>();
            let fd_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(received_fds.as_ptr() as *const u8, data_len)
            };
            let mut buf = CmsgBuffer {
                ptr: control_ptr,
                len: control_len,
                write_off: &mut write_off,
            };
            if let Err(e) = buf.put(
                &mut msg.msg_flags,
                SOL_SOCKET,
                SCM_RIGHTS,
                data_len,
                fd_bytes,
            ) {
                super::rollback_allocated_fds(&received_fds);
                return Err(e);
            }
        }

        msg.msg_controllen = write_off;
        Ok(ret_len)
    }

    fn send(&self, buffer: &[u8], _flags: socket::PMSG) -> Result<usize, SystemError> {
        let result = self.send_with_timeout(buffer, _flags);

        // Record boundaries + optional creds are needed for Linux-like recvmsg
        // coalescing semantics.
        if let Ok((sent, start, _written_len)) = result {
            if sent != 0 {
                let peer_passcred = self
                    .peer
                    .lock()
                    .as_ref()
                    .and_then(|w| w.upgrade())
                    .map(|p| p.passcred.load(core::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(false);
                let auto_attach =
                    self.passcred.load(core::sync::atomic::Ordering::Relaxed) || peer_passcred;
                let cred = if auto_attach {
                    Some(current_ucred())
                } else {
                    None
                };

                match self
                    .inner
                    .read()
                    .as_ref()
                    .expect("UnixStreamSocket inner is None")
                {
                    Inner::Connected(connected) => {
                        connected.push_scm_at(start, sent, cred, alloc::vec::Vec::new())
                    }
                    _ => return Err(SystemError::ENOTCONN),
                }
            }
        }

        // If send succeeded, notify peer's fasync_items for SIGIO
        if result.is_ok() && result.as_ref().unwrap().0 > 0 {
            if let Some(peer_weak) = self.peer.lock().as_ref() {
                if let Some(peer) = peer_weak.upgrade() {
                    peer.fasync_items.send_sigio();

                    // Wake EPOLLIN waiters on the peer. This is required for EPOLLET semantics
                    // in gVisor tests (a second write should re-trigger EPOLLIN even if the
                    // socket remains readable).
                    peer.wait_queue
                        .wakeup(Some(crate::process::ProcessState::Blocked(true)));
                    let _ = EventPoll::wakeup_epoll(
                        peer.epoll_items().as_ref(),
                        EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
                    );
                }
            }
        }

        result.map(|(n, _, _)| n)
    }

    fn send_msg(&self, _msg: &MsgHdr, _flags: socket::PMSG) -> Result<usize, SystemError> {
        let msg = _msg;

        if !msg.msg_name.is_null() {
            // Connected SOCK_STREAM does not accept a destination address.
            return Err(SystemError::EISCONN);
        }

        // Gather payload from user iovecs.
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let buf = iovs.gather()?;

        // Send/timeout semantics must match send(2), including EMSGSIZE checks.

        // Parse SCM_RIGHTS / SCM_CREDENTIALS from msg_control (if present).
        let mut rights_files: alloc::vec::Vec<
            alloc::sync::Arc<crate::filesystem::vfs::file::File>,
        > = alloc::vec::Vec::new();
        let mut force_creds = false;

        if !msg.msg_control.is_null() && msg.msg_controllen != 0 {
            if msg.msg_controllen < core::mem::size_of::<Cmsghdr>() {
                return Err(SystemError::EINVAL);
            }

            let reader =
                UserBufferReader::new(msg.msg_control as *const u8, msg.msg_controllen, true)?;
            let mut off = 0usize;
            while off + core::mem::size_of::<Cmsghdr>() <= msg.msg_controllen {
                let hdr = *reader.read_one_from_user::<Cmsghdr>(off)?;
                if hdr.cmsg_len < core::mem::size_of::<Cmsghdr>() {
                    return Err(SystemError::EINVAL);
                }
                if off + hdr.cmsg_len > msg.msg_controllen {
                    return Err(SystemError::EINVAL);
                }

                if hdr.cmsg_level == SOL_SOCKET && hdr.cmsg_type == SCM_RIGHTS {
                    let data_off = off + core::mem::size_of::<Cmsghdr>();
                    let data_len = hdr.cmsg_len - core::mem::size_of::<Cmsghdr>();
                    if !data_len.is_multiple_of(core::mem::size_of::<i32>()) {
                        return Err(SystemError::EINVAL);
                    }

                    let fd_count = data_len / core::mem::size_of::<i32>();
                    if fd_count != 0 {
                        let mut fds: alloc::vec::Vec<i32> = alloc::vec![0; fd_count];
                        reader.copy_from_user::<i32>(&mut fds, data_off)?;

                        let fd_table_binding = ProcessManager::current_pcb().fd_table();
                        let fd_table = fd_table_binding.read();
                        for fd in fds {
                            let file = fd_table.get_file_by_fd(fd).ok_or(SystemError::EBADF)?;
                            rights_files.push(file);
                        }
                    }
                }

                if hdr.cmsg_level == SOL_SOCKET && hdr.cmsg_type == SCM_CREDENTIALS {
                    let want = core::mem::size_of::<Cmsghdr>() + core::mem::size_of::<UCred>();
                    if hdr.cmsg_len != want {
                        return Err(SystemError::EINVAL);
                    }
                    force_creds = true;
                }

                off += cmsg_align(hdr.cmsg_len);
            }
        }

        // Send payload first; ancillary data is associated with the bytes sent.
        let (sent, start, _written_len) = self.send_with_timeout(&buf, _flags)?;

        // Notify peer on successful write.
        if sent > 0 {
            if let Some(peer_weak) = self.peer.lock().as_ref() {
                if let Some(peer) = peer_weak.upgrade() {
                    peer.fasync_items.send_sigio();
                    peer.wait_queue
                        .wakeup(Some(crate::process::ProcessState::Blocked(true)));
                    let _ = EventPoll::wakeup_epoll(
                        peer.epoll_items().as_ref(),
                        EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
                    );
                }
            }
        }

        if sent != 0 {
            let peer_passcred = self
                .peer
                .lock()
                .as_ref()
                .and_then(|w| w.upgrade())
                .map(|p| p.passcred.load(core::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false);
            let auto_attach =
                self.passcred.load(core::sync::atomic::Ordering::Relaxed) || peer_passcred;

            let attach_creds = force_creds || auto_attach;
            let cred = if attach_creds {
                Some(current_ucred())
            } else {
                None
            };

            match self
                .inner
                .read()
                .as_ref()
                .expect("UnixStreamSocket inner is None")
            {
                Inner::Connected(connected) => {
                    connected.push_scm_at(start, sent, cred, rights_files);
                }
                _ => return Err(SystemError::ENOTCONN),
            }
        }

        Ok(sent)
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: socket::PMSG,
        _address: Endpoint,
    ) -> Result<usize, SystemError> {
        // Linux AF_UNIX semantics (as exercised by gVisor):
        // - SOCK_STREAM: sendto() with a destination address on a connected socket -> EISCONN.
        //               sendto() on an unconnected socket -> EOPNOTSUPP (address is ignored).
        // - SOCK_SEQPACKET: sendto() ignores the destination address entirely.
        //                  Connected -> behaves like send(2).
        //                  Unconnected -> ENOTCONN.
        let connected = matches!(
            self.inner
                .read()
                .as_ref()
                .expect("UnixStreamSocket inner is None"),
            Inner::Connected(_)
        );

        if !connected {
            return Err(if self.is_seqpacket {
                SystemError::ENOTCONN
            } else {
                SystemError::EOPNOTSUPP_OR_ENOTSUP
            });
        }

        if self.is_seqpacket {
            // Ignore destination address.
            return self.send(buffer, flags);
        }

        // Connected stream socket with a destination address.
        let _ = _address;
        Err(SystemError::EISCONN)
    }

    fn send_buffer_size(&self) -> usize {
        self.sndbuf.load(Ordering::Relaxed)
    }

    fn recv_buffer_size(&self) -> usize {
        self.rcvbuf.load(Ordering::Relaxed)
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epitems
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        if !matches!(level, PSOL::SOCKET) {
            return Err(SystemError::ENOPROTOOPT);
        }

        let opt =
            crate::net::socket::PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
        match opt {
            crate::net::socket::PSO::SNDBUF => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.send_buffer_size() as u32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            crate::net::socket::PSO::RCVBUF => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.recv_buffer_size() as u32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            crate::net::socket::PSO::SNDTIMEO_OLD | crate::net::socket::PSO::SNDTIMEO_NEW => {
                Self::write_timeval(value, self.send_timeout_us.load(Ordering::Relaxed))
            }
            crate::net::socket::PSO::RCVTIMEO_OLD | crate::net::socket::PSO::RCVTIMEO_NEW => {
                Self::write_timeval(value, self.recv_timeout_us.load(Ordering::Relaxed))
            }
            crate::net::socket::PSO::PASSCRED => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v: i32 = if self.passcred.load(Ordering::Relaxed) {
                    1
                } else {
                    0
                };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            crate::net::socket::PSO::LINGER => {
                let linger = *self.linger.lock();
                linger.write_to(value)
            }
            crate::net::socket::PSO::ACCEPTCONN => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let is_listening = matches!(
                    self.inner
                        .read()
                        .as_ref()
                        .expect("UnixStreamSocket inner is None"),
                    Inner::Listener(_)
                );
                let v: i32 = if is_listening { 1 } else { 0 };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn do_close(&self) -> Result<(), SystemError> {
        // Close semantics for unix stream/seqpacket sockets:
        // - Mark both directions shutdown so peer sees EOF on read and EPIPE on write.
        // - Drop Listener to unregister backlog.
        let Some(inner) = self.inner.write().take() else {
            // Already closed.
            return Ok(());
        };

        match inner {
            Inner::Connected(connected) => {
                // Linux behavior: if this socket is closed with unread data in its
                // receive queue, the peer should observe ECONNRESET on the next read.
                if connected.readable_len(self.is_seqpacket) != 0 {
                    // Signal via the shared writer ring buffer so it works even when
                    // UnixStreamSocket.peer isn't wired up (e.g. filesystem/abstract bind).
                    connected.set_connreset_to_peer();

                    // Best-effort legacy path when peer Arc is known.
                    if let Some(peer_weak) = self.peer.lock().as_ref() {
                        if let Some(peer) = peer_weak.upgrade() {
                            peer.connreset_pending
                                .store(true, core::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }

                // Inform peer that we will no longer receive or send.
                connected.shutdown_recv();
                connected.shutdown_send();

                // Wake local waiters.
                self.wait_queue
                    .wakeup(Some(crate::process::ProcessState::Blocked(true)));

                // Wake peer waiters/epoll so blocking read/write can observe shutdown.
                if let Some(peer_weak) = self.peer.lock().as_ref() {
                    if let Some(peer) = peer_weak.upgrade() {
                        peer.wait_queue
                            .wakeup(Some(crate::process::ProcessState::Blocked(true)));
                        let _ = EventPoll::wakeup_epoll(
                            peer.epoll_items().as_ref(),
                            EPollEventType::EPOLLIN
                                | EPollEventType::EPOLLRDNORM
                                | EPollEventType::EPOLLHUP
                                | EPollEventType::EPOLLOUT
                                | EPollEventType::EPOLLWRNORM,
                        );
                        peer.fasync_items.send_sigio();
                    }
                }
            }
            Inner::Listener(_listener) => {
                // Drop unregisters the backlog.
            }
            Inner::Init(_init) => {
                // Nothing special.
            }
        }

        Ok(())
    }

    fn shutdown(&self, _how: common::ShutdownBit) -> Result<(), SystemError> {
        let inner_guard = self.inner.read();
        let Some(inner) = inner_guard.as_ref() else {
            return Err(SystemError::EINVAL);
        };

        let Inner::Connected(connected) = inner else {
            return Err(SystemError::ENOTCONN);
        };

        // For a connected unix socket, shutdown updates per-direction state:
        // - SHUT_RD: mark incoming direction as recv-shutdown (peer writes -> EPIPE)
        // - SHUT_WR: mark outgoing direction as send-shutdown (our writes -> EPIPE, peer reads -> EOF when drained)
        if _how.is_recv_shutdown() {
            connected.shutdown_recv();
        }
        if _how.is_send_shutdown() {
            connected.shutdown_send();
        }

        // Wake any sleepers.
        self.wait_queue
            .wakeup(Some(crate::process::ProcessState::Blocked(true)));

        Ok(())
    }

    fn check_io_event(&self) -> crate::filesystem::epoll::EPollEventType {
        self.inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
            .check_io_events()
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
}
