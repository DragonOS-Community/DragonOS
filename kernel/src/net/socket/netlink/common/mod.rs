use crate::{
    filesystem::{
        epoll::EPollEventType,
        vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId},
    },
    libs::{rwsem::RwSem, wait_queue::WaitQueue},
    net::socket::{
        endpoint::Endpoint,
        netlink::{
            addr::{multicast::GroupIdSet, NetlinkSocketAddr},
            common::{bound::BoundNetlink, unbound::UnboundNetlink},
            table::SupportedNetlinkProtocol,
        },
        utils::datagram_common::{select_remote_and_bind, Bound, Inner},
        Socket, PMSG,
    },
    process::{namespace::net_namespace::NetNamespace, ProcessManager},
};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize};
use system_error::SystemError;

pub(super) mod bound;
mod unbound;

#[derive(Debug)]
pub struct NetlinkSocket<P: SupportedNetlinkProtocol> {
    inner: RwSem<Inner<UnboundNetlink<P>, BoundNetlink<P::Message>>>,

    is_nonblocking: AtomicBool,
    wait_queue: Arc<WaitQueue>,
    netns: Arc<NetNamespace>,
    fasync_items: FAsyncItems,
    inode_id: InodeId,
    open_files: AtomicUsize,
}

impl<P: SupportedNetlinkProtocol> NetlinkSocket<P>
where
    BoundNetlink<P::Message>: Bound<Endpoint = NetlinkSocketAddr>,
{
    pub fn new(is_nonblocking: bool) -> Arc<Self> {
        let unbound = UnboundNetlink::new();
        Arc::new(Self {
            inner: RwSem::new(Inner::Unbound(unbound)),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            wait_queue: Arc::new(WaitQueue::default()),
            netns: ProcessManager::current_netns(),
            fasync_items: FAsyncItems::default(),
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
    ) -> Result<(usize, Endpoint), SystemError> {
        let (recv_bytes, endpoint) = self
            .inner
            .read()
            .try_recv(buf, flags)
            .map(|(recv_bytes, remote_endpoint)| (recv_bytes, remote_endpoint.into()))?;
        // todo self.pollee.invalidate();

        Ok((recv_bytes, endpoint))
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
        // log::info!("NetlinkSocket recv_from called");
        if let Some(addr) = address {
            self.connect(addr)?;
        }

        return if self.is_nonblocking() || flags.contains(PMSG::DONTWAIT) {
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
        };
        // self.try_recv(buffer, flags)
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

    fn recv_msg(
        &self,
        _msg: &mut crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        todo!("implement recv_msg for netlink socket");
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        self.try_send(buffer, None, flags)
    }

    fn send_msg(
        &self,
        _msg: &crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        todo!("implement send_msg for netlink socket");
    }

    fn do_close(&self) -> Result<(), SystemError> {
        //TODO close the socket properly
        Ok(())
    }

    fn epoll_items(&self) -> &crate::net::socket::common::EPollItems {
        todo!("implement epoll_items for netlink socket");
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        if let Some(addr) = self.inner.read().addr() {
            Ok(addr.into())
        } else {
            Err(SystemError::ENOTCONN)
        }
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        if let Some(addr) = self.inner.read().peer_addr() {
            Ok(addr.into())
        } else {
            Err(SystemError::ENOTCONN)
        }
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
}

impl<P: SupportedNetlinkProtocol> NetlinkSocket<P> {
    pub fn is_nonblocking(&self) -> bool {
        self.is_nonblocking
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    #[allow(unused)]
    pub fn set_nonblocking(&self, nonblocking: bool) {
        self.is_nonblocking
            .store(nonblocking, core::sync::atomic::Ordering::Relaxed);
    }
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
