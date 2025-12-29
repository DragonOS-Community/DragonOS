use inner::{UdpInner, UnboundUdp};
use smoltcp;
use system_error::SystemError;

use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::EPollItems;
use crate::net::socket::{Socket, PMSG};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;
use crate::{libs::rwlock::RwLock, net::socket::endpoint::Endpoint};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize};

use super::InetSocket;

pub mod inner;

type EP = crate::filesystem::epoll::EPollEventType;

// Udp Socket 负责提供状态切换接口、执行状态切换
#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct UdpSocket {
    inner: RwLock<Option<UdpInner>>,
    nonblock: AtomicBool,
    wait_queue: WaitQueue,
    inode_id: InodeId,
    open_files: AtomicUsize,
    self_ref: Weak<UdpSocket>,
    netns: Arc<NetNamespace>,
    epoll_items: EPollItems,
    fasync_items: FAsyncItems,
}

impl UdpSocket {
    pub fn new(nonblock: bool) -> Arc<Self> {
        let netns = ProcessManager::current_netns();
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(UdpInner::Unbound(UnboundUdp::new()))),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
        })
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut inner = self.inner.write();
        let prev = inner.take().ok_or(SystemError::EINVAL)?;
        match prev {
            UdpInner::Unbound(unbound) => match unbound.bind(local_endpoint, self.netns()) {
                Ok(bound) => {
                    bound
                        .inner()
                        .iface()
                        .common()
                        .bind_socket(self.self_ref.upgrade().unwrap());
                    *inner = Some(UdpInner::Bound(bound));
                    Ok(())
                }
                Err(e) => {
                    // bind 消费了 unbound（move）。失败时恢复到一个新的 Unbound 状态，
                    // 关键是避免 inner 变成 None 导致后续 check_io_event panic。
                    *inner = Some(UdpInner::Unbound(UnboundUdp::new()));
                    Err(e)
                }
            },
            other => {
                // 非 Unbound 情况下保持原状态
                *inner = Some(other);
                Err(SystemError::EINVAL)
            }
        }
    }

    pub fn bind_emphemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        let mut inner_guard = self.inner.write();
        let prev = inner_guard.take().ok_or(SystemError::EINVAL)?;
        match prev {
            UdpInner::Bound(bound) => {
                inner_guard.replace(UdpInner::Bound(bound));
                Ok(())
            }
            UdpInner::Unbound(unbound) => match unbound.bind_ephemeral(remote, self.netns()) {
                Ok(bound) => {
                    inner_guard.replace(UdpInner::Bound(bound));
                    Ok(())
                }
                Err(e) => {
                    // bind_ephemeral 消费了 unbound（move）。失败则恢复到新的 Unbound 状态。
                    inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                    Err(e)
                }
            },
        }
    }

    pub fn is_bound(&self) -> bool {
        let inner = self.inner.read();
        if let Some(UdpInner::Bound(_)) = &*inner {
            return true;
        }
        return false;
    }

    pub fn close(&self) {
        let mut inner = self.inner.write();
        if let Some(UdpInner::Bound(bound)) = &mut *inner {
            bound.close();
            inner.take();
        }
        // unbound socket just drop (only need to free memory)
    }

    pub fn try_recv(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpEndpoint), SystemError> {
        let guard = self.inner.read();
        match guard.as_ref() {
            Some(UdpInner::Bound(bound)) => {
                let ret = bound.try_recv(buf);
                bound.inner().iface().poll();
                ret
            }
            _ => Err(SystemError::ENOTCONN),
        }
    }

    #[inline]
    pub fn can_recv(&self) -> bool {
        self.check_io_event().contains(EP::EPOLLIN)
    }

    #[inline]
    #[allow(dead_code)]
    pub fn can_send(&self) -> bool {
        self.check_io_event().contains(EP::EPOLLOUT)
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        // 先确保 socket 处于 Bound 状态。任何错误路径都必须恢复 inner，避免变成 None。
        {
            let mut inner_guard = self.inner.write();
            let prev = inner_guard.take().ok_or(SystemError::EINVAL)?;
            match prev {
                UdpInner::Bound(bound) => {
                    inner_guard.replace(UdpInner::Bound(bound));
                }
                UdpInner::Unbound(unbound) => {
                    let Some(dest) = to.map(|ep| ep.addr) else {
                        // 必须恢复原状态，避免 inner=None。
                        inner_guard.replace(UdpInner::Unbound(unbound));
                        return Err(SystemError::EDESTADDRREQ);
                    };
                    match unbound.bind_ephemeral(dest, self.netns()) {
                        Ok(bound) => {
                            inner_guard.replace(UdpInner::Bound(bound));
                        }
                        Err(e) => {
                            // bind_ephemeral 消费了 unbound（move）。失败则恢复到新的 Unbound 状态。
                            inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                            return Err(e);
                        }
                    }
                }
            }
        }
        // Optimize: 拿两次锁的平均效率是否比一次长时间的读锁效率要高？
        let result = match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                let ret = bound.try_send(buf, to);
                bound.inner().iface().poll();
                ret
            }
            _ => Err(SystemError::ENOTCONN),
        };
        return result;
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }
}

impl Socket for UdpSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(local_endpoint) = local_endpoint {
            return self.do_bind(local_endpoint);
        }
        Err(SystemError::EAFNOSUPPORT)
    }

    fn send_buffer_size(&self) -> usize {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity())
            }
            _ => inner::DEFAULT_TX_BUF_SIZE,
        }
    }

    fn recv_buffer_size(&self) -> usize {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_recv_capacity())
            }
            _ => inner::DEFAULT_RX_BUF_SIZE,
        }
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(remote) = endpoint {
            if !self.is_bound() {
                self.bind_emphemeral(remote.addr)?;
            }
            if let UdpInner::Bound(inner) = self.inner.read().as_ref().expect("UDP Inner disappear")
            {
                inner.connect(remote);
                return Ok(());
            } else {
                panic!("");
            }
        }
        return Err(SystemError::EAFNOSUPPORT);
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if flags.contains(PMSG::DONTWAIT) {
            log::warn!("Nonblock send is not implemented yet");
        }

        return self.try_send(buffer, None);
    }

    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError> {
        if flags.contains(PMSG::DONTWAIT) {
            log::warn!("Nonblock send is not implemented yet");
        }

        if let Endpoint::Ip(remote) = address {
            return self.try_send(buffer, Some(remote));
        }

        return Err(SystemError::EINVAL);
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        return if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv(buffer)
        } else {
            loop {
                match self.try_recv(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    result => break result,
                }
            }
        }
        .map(|(len, _)| len);
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // could block io
        if let Some(endpoint) = address {
            self.connect(endpoint)?;
        }

        return if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv(buffer)
        } else {
            loop {
                match self.try_recv(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                        // log::debug!("UdpSocket::recv_from: wake up");
                    }
                    result => break result,
                }
            }
        }
        .map(|(len, remote)| (len, Endpoint::Ip(remote)));
    }

    fn do_close(&self) -> Result<(), SystemError> {
        self.close();
        Ok(())
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => Ok(Endpoint::Ip(bound.remote_endpoint()?)),
            // TODO: IPv6 support
            _ => Err(SystemError::ENOTCONN),
        }
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        use smoltcp::wire::{IpAddress::*, IpEndpoint, IpListenEndpoint};
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                let IpListenEndpoint { addr, port } = bound.endpoint();
                Ok(Endpoint::Ip(IpEndpoint::new(
                    addr.unwrap_or(Ipv4([0, 0, 0, 0].into())),
                    port,
                )))
            }
            // TODO: IPv6 support
            _ => Ok(Endpoint::Ip(IpEndpoint::new(Ipv4([0, 0, 0, 0].into()), 0))),
        }
    }

    fn recv_msg(
        &self,
        _msg: &mut crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn send_msg(
        &self,
        _msg: &crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn epoll_items(&self) -> &crate::net::socket::common::EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn check_io_event(&self) -> EPollEventType {
        let mut event = EPollEventType::empty();
        match self.inner.read().as_ref() {
            None | Some(UdpInner::Unbound(_)) => {
                event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
            }
            Some(UdpInner::Bound(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                }
            }
        }
        return event;
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
}

impl InetSocket for UdpSocket {
    fn on_iface_events(&self) {
        return;
    }
}

bitflags! {
    pub struct UdpSocketOptions: u32 {
        const ZERO = 0;        /* No UDP options */
        const UDP_CORK = 1;         /* Never send partially complete segments */
        const UDP_ENCAP = 100;      /* Set the socket to accept encapsulated packets */
        const UDP_NO_CHECK6_TX = 101; /* Disable sending checksum for UDP6X */
        const UDP_NO_CHECK6_RX = 102; /* Disable accepting checksum for UDP6 */
        const UDP_SEGMENT = 103;    /* Set GSO segmentation size */
        const UDP_GRO = 104;        /* This socket can receive UDP GRO packets */

        const UDPLITE_SEND_CSCOV = 10; /* sender partial coverage (as sent)      */
        const UDPLITE_RECV_CSCOV = 11; /* receiver partial coverage (threshold ) */
    }
}

bitflags! {
    pub struct UdpEncapTypes: u8 {
        const ZERO = 0;
        const ESPINUDP_NON_IKE = 1;     // draft-ietf-ipsec-nat-t-ike-00/01
        const ESPINUDP = 2;             // draft-ietf-ipsec-udp-encaps-06
        const L2TPINUDP = 3;            // rfc2661
        const GTP0 = 4;                 // GSM TS 09.60
        const GTP1U = 5;                // 3GPP TS 29.060
        const RXRPC = 6;
        const ESPINTCP = 7;             // Yikes, this is really xfrm encap types.
    }
}
