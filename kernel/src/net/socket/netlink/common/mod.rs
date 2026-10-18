use crate::{
    filesystem::{
        epoll::EPollEventType,
        vfs::{fasync::FAsyncItems, iov::IoVecs, vcore::generate_inode_id, InodeId},
    },
    libs::{rwsem::RwSem, wait_queue::WaitQueue},
    net::{
        posix::SockAddr,
        socket::{
            common::EPollItems,
            common::{
                parse_socket_buffer_size, parse_socket_buffer_size_force, SOCK_MIN_RCVBUF,
                SOCK_MIN_SNDBUF, SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX,
            },
            common::{
                parse_timeval_opt, parse_timeval_ticks, write_i32_getsockopt, write_timeval_opt,
                write_timeval_ticks, INFINITE_TIMEOUT_TICKS,
            },
            endpoint::Endpoint,
            netlink::{
                addr::{multicast::GroupIdSet, NetlinkSocketAddr},
                common::{bound::BoundNetlink, unbound::UnboundNetlink},
                receiver::MessageQueue,
                table::SupportedNetlinkProtocol,
            },
            utils::datagram_common::{Bound, Inner},
            AddressFamily, Socket, PMSG, PSO, PSOCK, PSOL,
        },
    },
    process::{
        cred::CAPFlags,
        namespace::{net_namespace::NetNamespace, user_namespace::INIT_USER_NAMESPACE},
        ProcessManager,
    },
    syscall::user_access::UserBufferReader,
};
use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use system_error::SystemError;

pub(super) mod bound;
mod unbound;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum NetlinkSockOpt {
    AddMembership = 1,
    DropMembership = 2,
    ListMemberships = 9,
}

impl TryFrom<u32> for NetlinkSockOpt {
    type Error = SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::AddMembership),
            2 => Ok(Self::DropMembership),
            9 => Ok(Self::ListMemberships),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
}

#[derive(Debug)]
pub struct NetlinkSocket<P: SupportedNetlinkProtocol> {
    inner: RwSem<Inner<UnboundNetlink<P>, BoundNetlink<P>>>,

    is_nonblocking: AtomicBool,
    wait_queue: Arc<WaitQueue>,
    epoll_items: Arc<EPollItems>,
    netns: Arc<NetNamespace>,
    socket_type: PSOCK,
    protocol: u32,
    group_count: AtomicUsize,
    send_timeout_us: AtomicU64,
    recv_timeout_ticks: AtomicU64,
    fasync_items: Arc<FAsyncItems>,
    inode_id: InodeId,
    open_files: AtomicUsize,
    fsnotify_watches: AtomicUsize,
    send_buffer: AtomicUsize,
    receive_queue: MessageQueue<P::Message>,
}

impl<P: SupportedNetlinkProtocol> NetlinkSocket<P>
where
    BoundNetlink<P>: Bound<Endpoint = NetlinkSocketAddr>,
{
    fn with_remote<R>(
        &self,
        to: Option<NetlinkSocketAddr>,
        op: impl FnOnce(&BoundNetlink<P>, NetlinkSocketAddr) -> Result<R, SystemError>,
    ) -> Result<R, SystemError> {
        loop {
            let inner = self.inner.read();
            self.check_open()?;
            if let Inner::Bound(bound) = &*inner {
                let remote = to
                    .or_else(|| bound.remote_endpoint())
                    .ok_or(SystemError::EDESTADDRREQ)?;
                return op(bound, remote);
            }
            drop(inner);
            let mut inner = self.inner.write();
            self.check_open()?;
            inner.bind_ephemeral(
                &NetlinkSocketAddr::new_unspecified(),
                self.wait_queue.clone(),
                self.netns(),
            )?;
        }
    }

    pub fn new(is_nonblocking: bool, socket_type: PSOCK, protocol: u32) -> Arc<Self> {
        let wait_queue = Arc::new(WaitQueue::default());
        let epoll_items = Arc::new(EPollItems::default());
        let fasync_items = Arc::new(FAsyncItems::default());
        let opener_cred = ProcessManager::current_pcb().cred();
        let receive_queue = P::new_message_queue();
        let unbound = UnboundNetlink::new(
            epoll_items.clone(),
            fasync_items.clone(),
            opener_cred,
            receive_queue.clone(),
        );
        Arc::new(Self {
            inner: RwSem::new(Inner::Unbound(unbound)),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            wait_queue,
            epoll_items,
            netns: ProcessManager::current_netns(),
            socket_type,
            protocol,
            group_count: AtomicUsize::new(0),
            send_timeout_us: AtomicU64::new(u64::MAX),
            recv_timeout_ticks: AtomicU64::new(INFINITE_TIMEOUT_TICKS),
            fasync_items,
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            fsnotify_watches: AtomicUsize::new(0),
            send_buffer: AtomicUsize::new(SYSCTL_WMEM_MAX as usize),
            receive_queue,
        })
    }

    fn try_send(
        &self,
        buf: &[u8],
        to: Option<NetlinkSocketAddr>,
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        self.check_send_len(buf.len())?;
        if P::userspace_send_supported() {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(buf.len())
                .map_err(|_| SystemError::ENOMEM)?;
            bytes.extend_from_slice(buf);
            return self.try_send_vec(bytes, to, flags);
        }
        let destination_is_explicit = to.is_some();
        if let Some(destination) = to.as_ref() {
            P::check_explicit_send(destination, &self.netns)?;
        }
        let send_bytes = self.with_remote(to, |bound, remote| {
            self.check_send_len(buf.len())?;
            bound.try_send(buf, &remote, flags, destination_is_explicit)
        })?;
        // todo pollee invalidate??

        Ok(send_bytes)
    }

    fn try_send_vec(
        &self,
        buf: Vec<u8>,
        to: Option<NetlinkSocketAddr>,
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        self.check_send_len(buf.len())?;
        let destination_is_explicit = to.is_some();
        if let Some(destination) = to.as_ref() {
            P::check_explicit_send(destination, &self.netns)?;
        }
        if P::userspace_send_supported() {
            // Resolve while bound, then release the sender lock before the
            // destination's receive-space wait. Close/connect need that lock.
            let remote = self.with_remote(to, |_bound, remote| Ok(remote))?;
            let timeout = self.send_timeout_us.load(Ordering::Relaxed);
            self.check_send_len(buf.len())?;
            return P::send_userspace(
                buf,
                remote,
                self.netns(),
                self.is_nonblocking() || flags.contains(PMSG::DONTWAIT),
                (timeout != u64::MAX).then(|| crate::time::Duration::from_micros(timeout)),
            );
        }
        let send_bytes = self.with_remote(to, |bound, remote| {
            self.check_send_len(buf.len())?;
            bound.try_send_vec(buf, &remote, flags, destination_is_explicit)
        })?;

        Ok(send_bytes)
    }

    fn try_recv_packet(&self, flags: PMSG) -> Result<P::Message, SystemError> {
        let inner = self.inner.read();
        self.check_open()?;
        match &*inner {
            Inner::Unbound(_) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Inner::Bound(bound) => bound.try_recv_packet(flags),
        }
    }

    #[inline]
    fn recv_return_len(copy_len: usize, orig_len: usize, flags: PMSG) -> usize {
        if flags.contains(PMSG::TRUNC) {
            orig_len
        } else {
            copy_len
        }
    }

    fn copy_netlink_user_buffer(
        &self,
        reader: &UserBufferReader<'_>,
        len: usize,
    ) -> Result<alloc::vec::Vec<u8>, SystemError> {
        self.check_send_len(len)?;
        crate::net::socket::base::copy_user_buffer_to_vec(reader, len)
    }

    fn check_open(&self) -> Result<(), SystemError> {
        if self.receive_queue.is_closed() {
            Err(SystemError::EBADF)
        } else {
            Ok(())
        }
    }

    fn check_send_len(&self, len: usize) -> Result<(), SystemError> {
        self.check_open()?;
        if len == 0 {
            return Err(SystemError::ENODATA);
        }
        if len > self.send_buffer.load(Ordering::Relaxed).saturating_sub(32) {
            return Err(SystemError::EMSGSIZE);
        }
        Ok(())
    }

    fn recv_from_inner(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
        address: Option<crate::net::socket::endpoint::Endpoint>,
    ) -> Result<(usize, usize, crate::net::socket::endpoint::Endpoint), system_error::SystemError>
    {
        let packet = self.recv_packet(flags, address)?;
        let bytes = P::message_bytes(&packet);
        let copied = buffer.len().min(bytes.len());
        buffer[..copied].copy_from_slice(&bytes[..copied]);
        Ok((copied, bytes.len(), P::message_source(&packet).into()))
    }

    fn recv_packet(
        &self,
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<P::Message, SystemError> {
        // Netlink does not support out-of-band data, even before bind. Reject
        // it before the unbound state can report an empty receive queue.
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if let Some(addr) = address {
            let endpoint = addr.try_into()?;
            P::check_connect(&endpoint, &self.netns)?;
            let mut inner = self.inner.write();
            self.check_open()?;
            inner.connect(&endpoint, self.wait_queue.clone(), self.netns())?;
        }

        if self.is_nonblocking() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv_packet(flags)
        } else {
            let timeout = self.recv_timeout_ticks.load(Ordering::Relaxed);
            let started = crate::time::timer::clock();
            loop {
                match self.try_recv_packet(flags) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        let finite = timeout != INFINITE_TIMEOUT_TICKS;
                        let wait = if finite {
                            let elapsed = crate::time::timer::clock().saturating_sub(started);
                            self.wait_queue.wait_event_io_interruptible_timeout_ticks(
                                || self.can_recv(),
                                timeout.saturating_sub(elapsed),
                            )
                        } else {
                            self.wait_queue
                                .wait_event_io_interruptible_timeout(|| self.can_recv(), None)
                        };
                        wait.map_err(|error| {
                            crate::net::socket::recv_timeout::map_wait_error(error, finite)
                        })?;
                    }
                    result => break result,
                }
            }
        }
    }

    /// 判断当前的netlink是否可以接收数据
    /// 目前netlink只是负责接收内核消息，所以暂时不用判断是否可以发送数据
    pub fn can_recv(&self) -> bool {
        if self.receive_queue.is_closed() {
            return true;
        }
        self.inner
            .read()
            .check_io_events()
            .intersects(EPollEventType::EPOLLIN | EPollEventType::EPOLLERR)
    }

    pub fn do_poll(&self) -> usize {
        self.inner.read().check_io_events().bits() as usize
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    fn ensure_membership_capacity(&self) {
        let target = P::multicast_group_count() as usize;
        let mut current = self.group_count.load(Ordering::Relaxed);
        while current < target {
            match self.group_count.compare_exchange(
                current,
                target,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn update_membership_capacity_for_bind(&self, addr: &NetlinkSocketAddr) {
        if !addr.groups().is_empty() {
            self.ensure_membership_capacity();
        }
    }

    fn list_memberships_needed_bytes(&self) -> usize {
        let groups = self.group_count.load(Ordering::Relaxed);
        groups.div_ceil(32) * core::mem::size_of::<u32>()
    }
}

impl<P: SupportedNetlinkProtocol + 'static> Socket for NetlinkSocket<P>
where
    BoundNetlink<P>: Bound<Endpoint = NetlinkSocketAddr>,
{
    fn netns(&self) -> Arc<NetNamespace> {
        NetlinkSocket::netns(self)
    }

    fn fsnotify_watch_counter(&self) -> &AtomicUsize {
        &self.fsnotify_watches
    }

    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn connect(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        let endpoint = endpoint.try_into()?;

        P::check_connect(&endpoint, &self.netns)?;

        let mut inner = self.inner.write();
        self.check_open()?;
        inner.connect(&endpoint, self.wait_queue.clone(), self.netns())
    }

    fn bind(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        let endpoint = endpoint.try_into()?;
        P::check_bind(&endpoint, &self.netns)?;
        self.update_membership_capacity_for_bind(&endpoint);

        let mut inner = self.inner.write();
        self.check_open()?;
        inner.bind(&endpoint, self.wait_queue.clone(), self.netns())
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: crate::net::socket::PMSG,
        address: crate::net::socket::endpoint::Endpoint,
    ) -> Result<usize, system_error::SystemError> {
        let endpoint = address.try_into()?;

        self.try_send(buffer, Some(endpoint), flags)
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
        address: Option<crate::net::socket::endpoint::Endpoint>,
    ) -> Result<(usize, crate::net::socket::endpoint::Endpoint), system_error::SystemError> {
        let (copy_len, orig_len, endpoint) = self.recv_from_inner(buffer, flags, address)?;
        Ok((Self::recv_return_len(copy_len, orig_len, flags), endpoint))
    }

    fn check_io_event(&self) -> crate::filesystem::epoll::EPollEventType {
        EPollEventType::from_bits_truncate(self.do_poll() as u32)
    }

    fn send_buffer_size(&self) -> usize {
        self.send_buffer.load(Ordering::Relaxed)
    }

    fn recv_buffer_size(&self) -> usize {
        self.receive_queue.receive_buffer()
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        let (len, _) = self.recv_from(buffer, flags, None)?;
        Ok(len)
    }

    fn read_to_user_buffer(
        &self,
        user_buffer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        if user_buffer.is_empty() {
            return Ok(0);
        }
        let packet = self.recv_packet(PMSG::empty(), None)?;
        user_buffer.write_all(P::message_bytes(&packet))
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
        iovs: &crate::syscall::iov::IoVecs,
    ) -> Result<usize, SystemError> {
        let packet = self.recv_packet(flags, None)?;
        let bytes = P::message_bytes(&packet);
        let orig_len = bytes.len();
        let copy_len = iovs.total_len().min(orig_len);
        iovs.scatter_exact(&bytes[..copy_len])?;
        let endpoint: Endpoint = P::message_source(&packet).into();

        if !msg.msg_name.is_null() {
            let actual_len = endpoint.write_to_user_msghdr(msg.msg_name, msg.msg_namelen)?;
            msg.msg_namelen = actual_len;
        } else {
            msg.msg_namelen = 0;
        }

        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        if orig_len > copy_len {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }
        Ok(Self::recv_return_len(copy_len, orig_len, flags))
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        self.try_send(buffer, None, flags)
    }

    fn send_user_buffer(
        &self,
        reader: &UserBufferReader<'_>,
        len: usize,
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<usize, SystemError> {
        let data = self.copy_netlink_user_buffer(reader, len)?;
        let sent = if let Some(endpoint) = address {
            let endpoint = endpoint.try_into()?;
            self.try_send_vec(data, Some(endpoint), flags)?
        } else {
            self.try_send_vec(data, None, flags)?
        };

        Ok(sent)
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                match opt {
                    PSO::ERROR => {
                        let error = self.receive_queue.take_error();
                        let value_errno = error.map_or(0, |error| -error.to_posix_errno());
                        Ok(write_i32_getsockopt(value, value_errno))
                    }
                    PSO::TYPE => {
                        let v = self.socket_type as i32;
                        Ok(write_i32_getsockopt(value, v))
                    }
                    PSO::SNDBUF => Ok(write_i32_getsockopt(value, self.send_buffer_size() as i32)),
                    PSO::RCVBUF => Ok(write_i32_getsockopt(value, self.recv_buffer_size() as i32)),
                    PSO::DOMAIN => {
                        let v = AddressFamily::Netlink as i32;
                        Ok(write_i32_getsockopt(value, v))
                    }
                    PSO::PROTOCOL => {
                        let v = self.protocol as i32;
                        Ok(write_i32_getsockopt(value, v))
                    }
                    PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW => {
                        let us = self.send_timeout_us.load(Ordering::Relaxed);
                        let us = if us == u64::MAX { 0 } else { us };
                        Ok(write_timeval_opt(value, us))
                    }
                    PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW => {
                        let ticks = self.recv_timeout_ticks.load(Ordering::Relaxed);
                        Ok(write_timeval_ticks(value, ticks))
                    }
                    _ => Err(SystemError::ENOPROTOOPT),
                }
            }
            PSOL::NETLINK => {
                let opt =
                    NetlinkSockOpt::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                match opt {
                    NetlinkSockOpt::ListMemberships => {
                        let groups: u64 = self
                            .inner
                            .read()
                            .addr()
                            .map_or(0, |addr| addr.groups().as_u64());
                        let needed = self.list_memberships_needed_bytes();
                        let groups_array = [groups as u32, (groups >> 32) as u32];
                        let bytes = unsafe {
                            core::slice::from_raw_parts(groups_array.as_ptr() as *const u8, needed)
                        };
                        let copy_len = core::cmp::min(value.len(), bytes.len());
                        value[..copy_len].copy_from_slice(&bytes[..copy_len]);
                        Ok(needed)
                    }
                    _ => Err(SystemError::ENOPROTOOPT),
                }
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn send_msg(&self, msg: &crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        self.check_send_len(iovs.total_len())?;
        let data = iovs.gather()?;
        let sent = if msg.msg_name.is_null() || msg.msg_namelen == 0 {
            self.try_send_vec(data, None, flags)?
        } else {
            let endpoint = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            let endpoint = endpoint.try_into()?;
            self.try_send_vec(data, Some(endpoint), flags)?
        };

        Ok(sent)
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                match opt {
                    PSO::SNDBUF | PSO::RCVBUF | PSO::SNDBUFFORCE | PSO::RCVBUFFORCE => {
                        // As with Linux sock_setsockopt, validate optlen before
                        // checking FORCE capability in the initial user namespace.
                        if val.len() < size_of::<i32>() {
                            return Err(SystemError::EINVAL);
                        }
                        let send = matches!(opt, PSO::SNDBUF | PSO::SNDBUFFORCE);
                        let force = matches!(opt, PSO::SNDBUFFORCE | PSO::RCVBUFFORCE);
                        let minimum = if send {
                            SOCK_MIN_SNDBUF
                        } else {
                            SOCK_MIN_RCVBUF
                        };
                        let size = if force {
                            if !ProcessManager::current_pcb()
                                .cred()
                                .has_capability_in_ns(&INIT_USER_NAMESPACE, CAPFlags::CAP_NET_ADMIN)
                            {
                                return Err(SystemError::EPERM);
                            }
                            parse_socket_buffer_size_force(val, minimum)?
                        } else {
                            parse_socket_buffer_size(
                                val,
                                if send {
                                    SYSCTL_WMEM_MAX
                                } else {
                                    SYSCTL_RMEM_MAX
                                },
                                minimum,
                            )?
                        };
                        if send {
                            self.send_buffer.store(size, Ordering::Relaxed);
                        } else {
                            self.receive_queue.set_receive_buffer(size);
                        }
                        Ok(())
                    }
                    PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW => {
                        let d = parse_timeval_opt(val)?;
                        let us = d.map(|v| v.total_micros()).unwrap_or(u64::MAX);
                        self.send_timeout_us.store(us, Ordering::Relaxed);
                        Ok(())
                    }
                    PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW => {
                        let ticks = parse_timeval_ticks(val)?.unwrap_or(INFINITE_TIMEOUT_TICKS);
                        self.recv_timeout_ticks.store(ticks, Ordering::Relaxed);
                        Ok(())
                    }
                    _ => Err(SystemError::ENOPROTOOPT),
                }
            }
            PSOL::NETLINK => {
                let opt =
                    NetlinkSockOpt::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                match opt {
                    NetlinkSockOpt::AddMembership => {
                        P::check_membership(&self.netns)?;
                        let groups = read_group_membership_sockopt::<P>(val)?;
                        self.ensure_membership_capacity();
                        self.inner.write().add_groups(groups);
                        Ok(())
                    }
                    NetlinkSockOpt::DropMembership => {
                        P::check_membership(&self.netns)?;
                        let groups = read_group_membership_sockopt::<P>(val)?;
                        self.ensure_membership_capacity();
                        self.inner.write().drop_groups(groups);
                        Ok(())
                    }
                    NetlinkSockOpt::ListMemberships => Err(SystemError::ENOPROTOOPT),
                }
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn do_close(&self) -> Result<(), SystemError> {
        // Dead first: pinned receivers must stop accepting before the port is
        // unregistered/reused. Drop the bound state to cancel dump work, too.
        self.receive_queue.close();
        let old = {
            let mut inner = self.inner.write();
            core::mem::replace(
                &mut *inner,
                Inner::Unbound(UnboundNetlink::new(
                    self.epoll_items.clone(),
                    self.fasync_items.clone(),
                    ProcessManager::current_pcb().cred(),
                    self.receive_queue.clone(),
                )),
            )
        };
        drop(old);
        self.wait_queue.wake_all();
        Ok(())
    }

    fn epoll_items(&self) -> &crate::net::socket::common::EPollItems {
        self.epoll_items.as_ref()
    }

    fn fasync_items(&self) -> &FAsyncItems {
        self.fasync_items.as_ref()
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        if let Some(addr) = self.inner.read().addr() {
            Ok(addr.into())
        } else {
            Err(SystemError::ENOTCONN)
        }
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        let peer = self
            .inner
            .read()
            .peer_addr()
            .unwrap_or(NetlinkSocketAddr::new_unspecified());
        Ok(peer.into())
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
}

impl<P: SupportedNetlinkProtocol> NetlinkSocket<P> {
    pub fn is_nonblocking(&self) -> bool {
        self.is_nonblocking.load(Ordering::Relaxed)
    }

    #[allow(unused)]
    pub fn set_nonblocking(&self, nonblocking: bool) {
        self.is_nonblocking.store(nonblocking, Ordering::Relaxed);
    }
}

fn read_u32_sockopt(val: &[u8]) -> Result<u32, SystemError> {
    if val.len() < size_of::<u32>() {
        return Err(SystemError::EINVAL);
    }

    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&val[..4]);
    Ok(u32::from_ne_bytes(bytes))
}

fn read_group_membership_sockopt<P: SupportedNetlinkProtocol>(
    val: &[u8],
) -> Result<GroupIdSet, SystemError> {
    let group_id = read_u32_sockopt(val)?;
    if group_id == 0 || group_id > P::multicast_group_count() {
        return Err(SystemError::EINVAL);
    }

    GroupIdSet::from_group_id(group_id).ok_or(SystemError::EINVAL)
}

// 多播消息的时候会用到，比如uevent
impl<P: SupportedNetlinkProtocol> Inner<UnboundNetlink<P>, BoundNetlink<P>> {
    #[allow(unused)]
    fn add_groups(&mut self, groups: GroupIdSet) {
        match self {
            Inner::Bound(bound) => bound.add_groups(groups),
            Inner::Unbound(unbound) => unbound.add_groups(groups),
        }
    }

    #[allow(unused)]
    fn drop_groups(&mut self, groups: GroupIdSet) {
        match self {
            Inner::Unbound(unbound) => unbound.drop_groups(groups),
            Inner::Bound(bound) => bound.drop_groups(groups),
        }
    }
}
