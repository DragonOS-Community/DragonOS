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
            common::{parse_timeval_opt, write_i32_getsockopt, write_timeval_opt},
            endpoint::Endpoint,
            netlink::{
                addr::{multicast::GroupIdSet, NetlinkSocketAddr},
                common::{bound::BoundNetlink, unbound::UnboundNetlink},
                table::SupportedNetlinkProtocol,
            },
            utils::datagram_common::{select_remote_and_bind, Bound, Inner},
            AddressFamily, Socket, PMSG, PSO, PSOCK, PSOL,
        },
    },
    process::{namespace::net_namespace::NetNamespace, ProcessManager},
};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    inner: RwSem<Inner<UnboundNetlink<P>, BoundNetlink<P::Message>>>,

    is_nonblocking: AtomicBool,
    wait_queue: Arc<WaitQueue>,
    epoll_items: Arc<EPollItems>,
    netns: Arc<NetNamespace>,
    socket_type: PSOCK,
    protocol: u32,
    group_count: AtomicUsize,
    send_timeout_us: AtomicU64,
    recv_timeout_us: AtomicU64,
    fasync_items: Arc<FAsyncItems>,
    inode_id: InodeId,
    open_files: AtomicUsize,
}

impl<P: SupportedNetlinkProtocol> NetlinkSocket<P>
where
    BoundNetlink<P::Message>: Bound<Endpoint = NetlinkSocketAddr>,
{
    pub fn new(is_nonblocking: bool, socket_type: PSOCK, protocol: u32) -> Arc<Self> {
        let wait_queue = Arc::new(WaitQueue::default());
        let epoll_items = Arc::new(EPollItems::default());
        let fasync_items = Arc::new(FAsyncItems::default());
        let unbound = UnboundNetlink::new(epoll_items.clone(), fasync_items.clone());
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
            recv_timeout_us: AtomicU64::new(u64::MAX),
            fasync_items,
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
        })
    }

    fn try_send(
        &self,
        buf: &[u8],
        to: Option<NetlinkSocketAddr>,
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        let send_bytes = select_remote_and_bind(
            &self.inner,
            to,
            || {
                self.inner.write().bind_ephemeral(
                    &NetlinkSocketAddr::new_unspecified(),
                    self.wait_queue.clone(),
                    self.netns(),
                )
            },
            |bound, remote| bound.try_send(buf, &remote, flags),
        )?;
        // todo pollee invalidate??

        Ok(send_bytes)
    }

    fn try_recv(
        &self,
        buf: &mut [u8],
        flags: crate::net::socket::PMSG,
    ) -> Result<(usize, usize, Endpoint), SystemError> {
        let (recv_bytes, orig_len, endpoint) = self.inner.read().try_recv(buf, flags).map(
            |(recv_bytes, orig_len, remote_endpoint)| {
                (recv_bytes, orig_len, remote_endpoint.into())
            },
        )?;
        // todo self.pollee.invalidate();

        Ok((recv_bytes, orig_len, endpoint))
    }

    #[inline]
    fn recv_return_len(copy_len: usize, orig_len: usize, flags: PMSG) -> usize {
        if flags.contains(PMSG::TRUNC) {
            orig_len
        } else {
            copy_len
        }
    }

    fn recv_from_inner(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
        address: Option<crate::net::socket::endpoint::Endpoint>,
    ) -> Result<(usize, usize, crate::net::socket::endpoint::Endpoint), system_error::SystemError>
    {
        if let Some(addr) = address {
            let endpoint = addr.try_into()?;
            self.inner
                .write()
                .connect(&endpoint, self.wait_queue.clone(), self.netns())?;
        }

        if self.is_nonblocking() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv(buffer, flags)
        } else {
            loop {
                match self.try_recv(buffer, flags) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        let _ = wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {});
                    }
                    result => break result,
                }
            }
        }
    }

    /// 判断当前的netlink是否可以接收数据
    /// 目前netlink只是负责接收内核消息，所以暂时不用判断是否可以发送数据
    pub fn can_recv(&self) -> bool {
        self.inner
            .read()
            .check_io_events()
            .contains(EPollEventType::EPOLLIN)
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
    BoundNetlink<P::Message>: Bound<Endpoint = NetlinkSocketAddr>,
{
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn connect(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        let endpoint = endpoint.try_into()?;

        self.inner
            .write()
            .connect(&endpoint, self.wait_queue.clone(), self.netns())
    }

    fn bind(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        let endpoint = endpoint.try_into()?;
        self.update_membership_capacity_for_bind(&endpoint);

        self.inner
            .write()
            .bind(&endpoint, self.wait_queue.clone(), self.netns())
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
        // log::warn!("send_buffer_size is implemented to 0");
        // netlink sockets typically do not have a send buffer size like stream sockets.
        0
    }

    fn recv_buffer_size(&self) -> usize {
        // log::warn!("recv_buffer_size is implemented to 0");
        // netlink sockets typically do not have a recv buffer size like stream sockets.
        0
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        let (len, _) = self.recv_from(buffer, flags, None)?;
        Ok(len)
    }

    fn read_to_user_buffer(
        &self,
        user_buffer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        const NETLINK_READ_SCRATCH: usize = 64 * 1024;
        crate::net::socket::base::read_to_user_buffer_via_kernel_buf(
            self,
            user_buffer,
            NETLINK_READ_SCRATCH,
        )
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let mut buf = iovs.new_buf(true);

        let (copy_len, orig_len, endpoint) = self.recv_from_inner(&mut buf, flags, None)?;
        iovs.scatter(&buf[..copy_len])?;

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

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                match opt {
                    PSO::TYPE => {
                        let v = self.socket_type as i32;
                        Ok(write_i32_getsockopt(value, v))
                    }
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
                        let us = self.recv_timeout_us.load(Ordering::Relaxed);
                        let us = if us == u64::MAX { 0 } else { us };
                        Ok(write_timeval_opt(value, us))
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
        let data = iovs.gather()?;

        if msg.msg_name.is_null() || msg.msg_namelen == 0 {
            self.send(&data, flags)
        } else {
            let endpoint = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            self.send_to(&data, flags, endpoint)
        }
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                match opt {
                    PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW => {
                        let d = parse_timeval_opt(val)?;
                        let us = d.map(|v| v.total_micros()).unwrap_or(u64::MAX);
                        self.send_timeout_us.store(us, Ordering::Relaxed);
                        Ok(())
                    }
                    PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW => {
                        let d = parse_timeval_opt(val)?;
                        let us = d.map(|v| v.total_micros()).unwrap_or(u64::MAX);
                        self.recv_timeout_us.store(us, Ordering::Relaxed);
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
                        let groups = read_group_membership_sockopt::<P>(val)?;
                        self.ensure_membership_capacity();
                        self.inner.write().add_groups(groups);
                        Ok(())
                    }
                    NetlinkSockOpt::DropMembership => {
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
        //TODO close the socket properly
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
impl<P: SupportedNetlinkProtocol> Inner<UnboundNetlink<P>, BoundNetlink<P::Message>> {
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
