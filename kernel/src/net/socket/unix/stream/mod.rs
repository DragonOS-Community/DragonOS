use crate::{
    filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, FilePrivateData, InodeId},
    libs::rwlock::RwLock,
    net::socket::{self, *},
};
use crate::{
    libs::spinlock::SpinLock,
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
use core::sync::atomic::AtomicBool;
use inner::{Connected, Init, Inner};
use log::debug;
use system_error::SystemError;

use crate::filesystem::vfs::iov::IoVecs;
use crate::process::ProcessManager;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Cmsghdr {
    cmsg_len: usize,
    cmsg_level: i32,
    cmsg_type: i32,
}

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;

// Linux: MSG_CTRUNC is 0x8.
const MSG_CTRUNC: i32 = 0x8;

fn cmsg_align(len: usize) -> usize {
    let align = core::mem::size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

// Socket ioctls used by gVisor unix socket tests.
const TIOCOUTQ: u32 = 0x5411; // Get output queue size
const FIONREAD: u32 = 0x541B; // Get input queue size (aka TIOCINQ)
const SIOCGIFINDEX: u32 = 0x8933; // name -> if_index mapping

fn clamp_usize_to_i32(v: usize) -> i32 {
    core::cmp::min(v, i32::MAX as usize) as i32
}

pub mod inner;

#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct UnixStreamSocket {
    inner: RwLock<Option<Inner>>,
    //todo options
    epitems: EPollItems,
    fasync_items: FAsyncItems,
    wait_queue: Arc<WaitQueue>,
    inode_id: InodeId,
    /// Peer socket for socket pairs (used for SIGIO notification)
    peer: SpinLock<Option<Weak<UnixStreamSocket>>>,

    is_nonblocking: AtomicBool,
    is_seqpacket: bool,
}

impl UnixStreamSocket {
    /// 默认的元数据缓冲区大小
    #[allow(dead_code)]
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub(super) fn new_init(init: Init, is_nonblocking: bool, is_seqpacket: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Some(Inner::Init(init))),
            wait_queue: Arc::new(WaitQueue::default()),
            inode_id: generate_inode_id(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            peer: SpinLock::new(None),
        })
    }

    pub(super) fn new_connected(
        connected: Connected,
        is_nonblocking: bool,
        is_seqpacket: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Some(Inner::Connected(connected))),
            wait_queue: Arc::new(WaitQueue::default()),
            inode_id: generate_inode_id(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            peer: SpinLock::new(None),
        })
    }

    pub fn new(is_nonblocking: bool, is_seqpacket: bool) -> Arc<Self> {
        Self::new_init(Init::new(), is_nonblocking, is_seqpacket)
    }

    pub fn new_pair(is_nonblocking: bool, is_seqpacket: bool) -> (Arc<Self>, Arc<Self>) {
        let (conn_a, conn_b) = Connected::new_pair(None, None);
        let socket_a = Self::new_connected(conn_a, is_nonblocking, is_seqpacket);
        let socket_b = Self::new_connected(conn_b, is_nonblocking, is_seqpacket);

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

    fn try_send(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        match self.inner.read().as_ref().expect("inner is None") {
            Inner::Connected(connected) => connected.try_send(buffer, self.is_seqpacket),
            _ => {
                log::error!("the socket is not connected");
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
            Inner::Init(init) => match backlog.push_incoming(init, self.is_seqpacket) {
                Ok(connected) => (Inner::Connected(connected), Ok(())),
                Err((init, err)) => (Inner::Init(init), Err(err)),
            },
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
            Inner::Listener(listener) => listener.try_accept(self.is_seqpacket) as _,
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
}

impl Socket for UnixStreamSocket {
    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        if arg == 0 {
            return Err(SystemError::EFAULT);
        }

        match cmd {
            // Return bytes available for reading.
            FIONREAD => {
                let available = self.ioctl_fionread();
                let mut writer =
                    UserBufferWriter::new(arg as *mut u8, core::mem::size_of::<i32>(), true)?;
                writer
                    .buffer_protected(0)?
                    .write_one::<i32>(0, &clamp_usize_to_i32(available))?;
                Ok(0)
            }
            // Return bytes queued for transmission.
            TIOCOUTQ => {
                let queued = self.ioctl_tiocoutq();
                let mut writer =
                    UserBufferWriter::new(arg as *mut u8, core::mem::size_of::<i32>(), true)?;
                writer
                    .buffer_protected(0)?
                    .write_one::<i32>(0, &clamp_usize_to_i32(queued))?;
                Ok(0)
            }
            // Netdevice ioctls on AF_UNIX sockets: gVisor tests accept ENODEV.
            SIOCGIFINDEX => Err(SystemError::ENODEV),
            _ => Err(SystemError::ENOSYS),
        }
    }

    fn connect(&self, server_endpoint: Endpoint) -> Result<(), SystemError> {
        let remote_addr = UnixEndpoint::try_from(server_endpoint)?.connect()?;
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
            Inner::Init(init) => init.bind(addr),
            Inner::Connected(connected) => connected.bind(addr),
            Inner::Listener(_listener) => addr.bind_unnamed(),
        }
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        const SOMAXCONN: usize = 4096;
        let backlog = backlog.saturating_add(1).min(SOMAXCONN);

        let mut writer = self.inner.write();

        let (inner, err) = match writer.take().expect("UnixStreamSocket inner is None") {
            Inner::Init(init) => {
                match init.listen(backlog, self.is_seqpacket, self.wait_queue.clone()) {
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
        debug!("stream server begin accept");
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

    fn set_option(&self, _level: PSOL, _optname: usize, _optval: &[u8]) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
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
        self.try_recv(buffer)
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        _flags: socket::PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // 对于流式 Unix Socket，recv_from 与 recv 类似
        // 直接调用 try_recv 并返回对端地址
        let recv_len = self.try_recv(buffer)?;

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
        let recv_size = match self.recv(&mut buf, _flags) {
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        if recv_size != 0 {
            iovs.scatter(&buf[..recv_size])?;
        }

        // Default: no flags, no control.
        msg.msg_flags = 0;
        msg.msg_controllen = 0;

        // Try to deliver SCM_RIGHTS for the next message (if any).
        let rights = match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected.pop_scm_rights(),
            _ => None,
        };

        let Some(rights) = rights else {
            return Ok(recv_size);
        };

        // If userspace didn't provide a control buffer, drop rights and report truncation.
        if control_ptr.is_null() || control_len == 0 {
            msg.msg_flags |= MSG_CTRUNC;
            return Ok(recv_size);
        }

        // Allocate new fds in the receiver. Note: DragonOS models close-on-exec on File,
        // so we clone file objects and override close_on_exec based on MSG_CMSG_CLOEXEC.
        let cloexec = _flags.contains(socket::PMSG::CMSG_CLOEXEC);
        let mut received_fds: alloc::vec::Vec<i32> = alloc::vec::Vec::with_capacity(rights.len());
        {
            let fd_table_binding = ProcessManager::current_pcb().fd_table();
            let mut fd_table = fd_table_binding.write();
            for file in rights.iter() {
                let new_file = file.as_ref().try_clone().ok_or(SystemError::EINVAL)?;
                new_file.set_close_on_exec(cloexec);
                let new_fd = fd_table.alloc_fd(new_file, None)?;
                received_fds.push(new_fd);
            }
        }

        // Serialize cmsghdr + int[] into msg_control.
        let data_len = received_fds.len() * core::mem::size_of::<i32>();
        let hdr_len = core::mem::size_of::<Cmsghdr>();
        let cmsg_len = hdr_len + data_len;
        let needed = cmsg_align(cmsg_len);
        if control_len < needed {
            // Not enough space to report fds; indicate truncation.
            msg.msg_flags |= MSG_CTRUNC;
            msg.msg_controllen = 0;
            return Ok(recv_size);
        }

        let hdr = Cmsghdr {
            cmsg_len,
            cmsg_level: SOL_SOCKET,
            cmsg_type: SCM_RIGHTS,
        };
        {
            let mut hdr_writer =
                UserBufferWriter::new(control_ptr, core::mem::size_of::<Cmsghdr>(), true)?;
            let mut protected = hdr_writer.buffer_protected(0)?;
            protected.write_one::<Cmsghdr>(0, &hdr)?;
        }

        // Write fd array right after header.
        let fds_off = hdr_len;
        for (idx, fd) in received_fds.iter().enumerate() {
            let off = fds_off + idx * core::mem::size_of::<i32>();
            let ptr = unsafe { control_ptr.add(off) };
            let mut fd_writer = UserBufferWriter::new(ptr, core::mem::size_of::<i32>(), true)?;
            let mut protected = fd_writer.buffer_protected(0)?;
            protected.write_one::<i32>(0, fd)?;
        }
        msg.msg_controllen = cmsg_len;

        Ok(recv_size)
    }

    fn send(&self, buffer: &[u8], _flags: socket::PMSG) -> Result<usize, SystemError> {
        let result = self.try_send(buffer);

        // If send succeeded, notify peer's fasync_items for SIGIO
        if result.is_ok() && result.as_ref().unwrap() > &0 {
            if let Some(peer_weak) = self.peer.lock().as_ref() {
                if let Some(peer) = peer_weak.upgrade() {
                    peer.fasync_items.send_sigio();
                }
            }
        }

        result
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

        // Parse SCM_RIGHTS from msg_control (if present).
        let mut rights_files: alloc::vec::Vec<
            alloc::sync::Arc<crate::filesystem::vfs::file::File>,
        > = alloc::vec::Vec::new();

        if !msg.msg_control.is_null() && msg.msg_controllen >= core::mem::size_of::<Cmsghdr>() {
            let reader =
                UserBufferReader::new(msg.msg_control as *const u8, msg.msg_controllen, true)?;
            let mut off = 0usize;
            while off + core::mem::size_of::<Cmsghdr>() <= msg.msg_controllen {
                let hdr = *reader.read_one_from_user::<Cmsghdr>(off)?;
                if hdr.cmsg_len < core::mem::size_of::<Cmsghdr>() {
                    break;
                }
                if off + hdr.cmsg_len > msg.msg_controllen {
                    break;
                }

                if hdr.cmsg_level == SOL_SOCKET && hdr.cmsg_type == SCM_RIGHTS {
                    let data_off = off + core::mem::size_of::<Cmsghdr>();
                    let data_len = hdr.cmsg_len - core::mem::size_of::<Cmsghdr>();
                    if data_len % core::mem::size_of::<i32>() != 0 {
                        return Err(SystemError::EINVAL);
                    }

                    let fd_count = data_len / core::mem::size_of::<i32>();
                    if fd_count != 0 {
                        let mut fds: alloc::vec::Vec<i32> =
                            alloc::vec::Vec::with_capacity(fd_count);
                        fds.resize(fd_count, 0);
                        reader.copy_from_user::<i32>(&mut fds, data_off)?;

                        let fd_table_binding = ProcessManager::current_pcb().fd_table();
                        let fd_table = fd_table_binding.read();
                        for fd in fds {
                            let file = fd_table.get_file_by_fd(fd).ok_or(SystemError::EBADF)?;
                            rights_files.push(file);
                        }
                    }
                }

                off += cmsg_align(hdr.cmsg_len);
            }
        }

        // Send payload first; only attach SCM_RIGHTS if the send succeeds.
        let sent = self.send(&buf, _flags)?;

        if !rights_files.is_empty() {
            match self
                .inner
                .read()
                .as_ref()
                .expect("UnixStreamSocket inner is None")
            {
                Inner::Connected(connected) => connected.push_scm_rights(rights_files),
                _ => return Err(SystemError::ENOTCONN),
            }
        }

        Ok(sent)
    }

    fn send_to(
        &self,
        _buffer: &[u8],
        _flags: socket::PMSG,
        _address: Endpoint,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send_buffer_size(&self) -> usize {
        log::warn!("using default buffer size");
        UnixStreamSocket::DEFAULT_BUF_SIZE
    }

    fn recv_buffer_size(&self) -> usize {
        log::warn!("using default buffer size");
        UnixStreamSocket::DEFAULT_BUF_SIZE
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epitems
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn option(&self, _level: PSOL, _name: usize, _value: &mut [u8]) -> Result<usize, SystemError> {
        Err(SystemError::ENOPROTOOPT)
    }

    fn do_close(&self) -> Result<(), SystemError> {
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
