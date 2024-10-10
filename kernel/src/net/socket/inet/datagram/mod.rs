use inet::InetSocket;
use smoltcp;
use system_error::SystemError::{self, *};

use crate::filesystem::vfs::IndexNode;
use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::SpinLock;
use crate::net::event_poll::EPollEventType;
use crate::net::net_core::poll_ifaces;
use crate::net::socket::*;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::AtomicBool;

pub mod inner;

use inner::*;

type EP = EPollEventType;

// Udp Socket 负责提供状态切换接口、执行状态切换
#[derive(Debug)]
pub struct UdpSocket {
    inner: RwLock<Option<UdpInner>>,
    nonblock: AtomicBool,
    wait_queue: WaitQueue,
    self_ref: Weak<UdpSocket>,
}

impl UdpSocket {
    pub fn new(nonblock: bool) -> Arc<Self> {
        return Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(UdpInner::Unbound(UnboundUdp::new()))),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
        });
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut inner = self.inner.write();
        if let Some(UdpInner::Unbound(unbound)) = inner.take() {
            let bound = unbound.bind(local_endpoint)?;

            bound
                .inner()
                .iface()
                .common()
                .bind_socket(self.self_ref.upgrade().unwrap());
            *inner = Some(UdpInner::Bound(bound));
            return Ok(());
        }
        return Err(EINVAL);
    }

    pub fn bind_emphemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        let mut inner_guard = self.inner.write();
        let bound = match inner_guard.take().expect("Udp inner is None") {
            UdpInner::Bound(inner) => inner,
            UdpInner::Unbound(inner) => inner.bind_ephemeral(remote)?,
        };
        inner_guard.replace(UdpInner::Bound(bound));
        return Ok(());
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
    }

    pub fn try_recv(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpEndpoint), SystemError> {
        poll_ifaces();
        let received = match self.inner.read().as_ref().expect("Udp Inner is None") {
            UdpInner::Bound(bound) => bound.try_recv(buf),
            _ => Err(ENOTCONN),
        };
        return received;
    }

    #[inline]
    pub fn can_recv(&self) -> bool {
        self.on_events().contains(EP::EPOLLIN)
    }

    #[inline]
    pub fn can_send(&self) -> bool {
        self.on_events().contains(EP::EPOLLOUT)
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        {
            let mut inner_guard = self.inner.write();
            let inner = match inner_guard.take().expect("Udp Inner is None") {
                UdpInner::Bound(bound) => bound,
                UdpInner::Unbound(unbound) => {
                    unbound.bind_ephemeral(to.ok_or(EADDRNOTAVAIL)?.addr)?
                }
            };
            // size = inner.try_send(buf, to)?;
            inner_guard.replace(UdpInner::Bound(inner));
        };
        // Optimize: 拿两次锁的平均效率是否比一次长时间的读锁效率要高？
        let result = match self.inner.read().as_ref().expect("Udp Inner is None") {
            UdpInner::Bound(bound) => bound.try_send(buf, to),
            _ => Err(ENOTCONN),
        };
        poll_ifaces();
        return result;
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if self.is_nonblock() {
            return self.try_recv(buf).map(|(size, _)| size);
        } else {
            // return self
            //     .wait_queue
            //     .busy_wait(EP::EPOLLIN, || self.try_recv(buf).map(|(size, _)| size));
            todo!()
        }
    }

    pub fn on_events(&self) -> EPollEventType {
        let mut event = EPollEventType::empty();
        match self.inner.read().as_ref().unwrap() {
            UdpInner::Unbound(_) => {
                event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
            }
            UdpInner::Bound(bound) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                } else {
                    todo!("缓冲区空间不够，需要使用信号处理");
                }
            }
        }
        return event;
    }
}

impl Socket for UdpSocket {
    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn poll(&self) -> usize {
        self.on_events().bits() as usize
    }

    fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(local_endpoint) = local_endpoint {
            return self.do_bind(local_endpoint);
        }
        Err(EAFNOSUPPORT)
    }

    fn send_buffer_size(&self) -> usize {
        match self.inner.read().as_ref().unwrap() {
            UdpInner::Bound(bound) => bound.with_socket(|socket| socket.payload_send_capacity()),
            _ => inner::DEFAULT_TX_BUF_SIZE,
        }
    }

    fn recv_buffer_size(&self) -> usize {
        match self.inner.read().as_ref().unwrap() {
            UdpInner::Bound(bound) => bound.with_socket(|socket| socket.payload_recv_capacity()),
            _ => inner::DEFAULT_RX_BUF_SIZE,
        }
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(remote) = endpoint {
            self.bind_emphemeral(remote.addr)?;
            if let UdpInner::Bound(inner) = self.inner.read().as_ref().expect("UDP Inner disappear")
            {
                inner.connect(remote);
                return Ok(());
            } else {
                panic!("");
            }
        }
        return Err(EAFNOSUPPORT);
    }

    fn send(&self, buffer: &[u8], flags: MessageFlag) -> Result<usize, SystemError> {
        // if flags.contains(MessageFlag::DONTWAIT) {

        return self.try_send(buffer, None);
        // } else {
        //     // return self
        //     //     .wait_queue
        //     //     .busy_wait(EP::EPOLLOUT, || self.try_send(buffer, None));
        //     todo!()
        // }
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: MessageFlag,
        address: Endpoint,
    ) -> Result<usize, SystemError> {
        // if flags.contains(MessageFlag::DONTWAIT) {
        if let Endpoint::Ip(remote) = address {
            return self.try_send(buffer, Some(remote));
        }
        // } else {
        //     // return self
        //     //     .wait_queue
        //     //     .busy_wait(EP::EPOLLOUT, || {
        //     //         if let Endpoint::Ip(remote) = address {
        //     //             return self.try_send(buffer, Some(remote.addr));
        //     //         }
        //     //         return Err(EAFNOSUPPORT);
        //     //     });
        //     todo!()
        // }
        return Err(EINVAL);
    }

    fn recv(&self, buffer: &mut [u8], flags: MessageFlag) -> Result<usize, SystemError> {
        use crate::sched::SchedMode;

        return if self.is_nonblock() || flags.contains(MessageFlag::DONTWAIT) {
            self.try_recv(buffer)
        } else {
            loop {
                match self.try_recv(buffer) {
                    Err(EAGAIN_OR_EWOULDBLOCK) => {
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
        flags: MessageFlag,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        use crate::sched::SchedMode;
        // could block io
        if let Some(endpoint) = address {
            self.connect(endpoint)?;
        }

        return if self.is_nonblock() || flags.contains(MessageFlag::DONTWAIT) {
            self.try_recv(buffer)
        } else {
            loop {
                match self.try_recv(buffer) {
                    Err(EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                        log::debug!("UdpSocket::recv_from: wake up");
                    }
                    result => break result,
                }
            }
        }
        .map(|(len, remote)| (len, Endpoint::Ip(remote)));
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

// fn sock_set_option(
//     &self,
//     _socket: &mut udp::Socket,
//     _level: SocketOptionsLevel,
//     optname: PosixSocketOption,
//     _optval: &[u8],
// ) -> Result<(), SystemError> {
//     use PosixSocketOption::*;
//     use SystemError::*;

//     if optname == SO_BINDTODEVICE {
//         todo!("SO_BINDTODEVICE");
//     }

//     match optname {
//         SO_TYPE => {}
//         SO_PROTOCOL => {}
//         SO_DOMAIN => {}
//         SO_ERROR => {
//             return Err(ENOPROTOOPT);
//         }
//         SO_TIMESTAMP_OLD => {}
//         SO_TIMESTAMP_NEW => {}
//         SO_TIMESTAMPNS_OLD => {}

//         SO_TIMESTAMPING_OLD => {}

//         SO_RCVTIMEO_OLD => {}

//         SO_SNDTIMEO_OLD => {}

//         // if define CONFIG_NET_RX_BUSY_POLL
//         SO_BUSY_POLL | SO_PREFER_BUSY_POLL | SO_BUSY_POLL_BUDGET => {
//             debug!("Unsupported socket option: {:?}", optname);
//             return Err(ENOPROTOOPT);
//         }
//         // end if
//         optname => {
//             debug!("Unsupported socket option: {:?}", optname);
//             return Err(ENOPROTOOPT);
//         }
//     }
//     return Ok(());
// }

// fn udp_set_option(
//     &self,
//     level: SocketOptionsLevel,
//     optname: usize,
//     optval: &[u8],
// ) -> Result<(), SystemError> {
//     use PosixSocketOption::*;

//     let so_opt_name =
//         PosixSocketOption::try_from(optname as i32)
//             .map_err(|_| SystemError::ENOPROTOOPT)?;

//     if level == SocketOptionsLevel::SOL_SOCKET {
//         self.with_mut_socket(f)
//         self.sock_set_option(self., level, so_opt_name, optval)?;
//         if so_opt_name == SO_RCVBUF || so_opt_name == SO_RCVBUFFORCE {
//             todo!("SO_RCVBUF");
//         }
//     }

//     match UdpSocketOptions::from_bits_truncate(optname as u32) {
//         UdpSocketOptions::UDP_CORK => {
//             todo!("UDP_CORK");
//         }
//         UdpSocketOptions::UDP_ENCAP => {
//             match UdpEncapTypes::from_bits_truncate(optval[0]) {
//                 UdpEncapTypes::ESPINUDP_NON_IKE => {
//                     todo!("ESPINUDP_NON_IKE");
//                 }
//                 UdpEncapTypes::ESPINUDP => {
//                     todo!("ESPINUDP");
//                 }
//                 UdpEncapTypes::L2TPINUDP => {
//                     todo!("L2TPINUDP");
//                 }
//                 UdpEncapTypes::GTP0 => {
//                     todo!("GTP0");
//                 }
//                 UdpEncapTypes::GTP1U => {
//                     todo!("GTP1U");
//                 }
//                 UdpEncapTypes::RXRPC => {
//                     todo!("RXRPC");
//                 }
//                 UdpEncapTypes::ESPINTCP => {
//                     todo!("ESPINTCP");
//                 }
//                 UdpEncapTypes::ZERO => {}
//                 _ => {
//                     return Err(SystemError::ENOPROTOOPT);
//                 }
//             }
//         }
//         UdpSocketOptions::UDP_NO_CHECK6_TX => {
//             todo!("UDP_NO_CHECK6_TX");
//         }
//         UdpSocketOptions::UDP_NO_CHECK6_RX => {
//             todo!("UDP_NO_CHECK6_RX");
//         }
//         UdpSocketOptions::UDP_SEGMENT => {
//             todo!("UDP_SEGMENT");
//         }
//         UdpSocketOptions::UDP_GRO => {
//             todo!("UDP_GRO");
//         }

//         UdpSocketOptions::UDPLITE_RECV_CSCOV => {
//             todo!("UDPLITE_RECV_CSCOV");
//         }
//         UdpSocketOptions::UDPLITE_SEND_CSCOV => {
//             todo!("UDPLITE_SEND_CSCOV");
//         }

//         UdpSocketOptions::ZERO => {}
//         _ => {
//             return Err(SystemError::ENOPROTOOPT);
//         }
//     }
//     return Ok(());
// }
