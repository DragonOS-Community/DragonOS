use alloc::{boxed::Box, sync::Arc, vec::Vec};
use log::{debug, error, warn};
use smoltcp::{
    socket::{raw, tcp, udp},
    wire,
};
use system_error::SystemError;

use crate::{
    driver::net::Iface,
    libs::rwlock::RwLock,
    net::{
        event_poll::EPollEventType, net_core::poll_ifaces, socket::tcp_def::TcpOptions, syscall::PosixSocketOption, Endpoint, Protocol, ShutdownType, NET_DEVICES, SocketOptionsLevel
    },
};

use crate::net::socket::{
    handle::GlobalSocketHandle, PosixSocketHandleItem, Socket, SocketHandleItem, SocketMetadata,
    SocketOptions, SocketPollMethod, SocketType, HANDLE_MAP, PORT_MANAGER, ip_def::IpOptions,
};


/// @brief 表示udp socket
///
/// https://man7.org/linux/man-pages/man7/udp.7.html
#[derive(Debug, Clone)]
pub struct UdpSocket {
    pub handle: GlobalSocketHandle,
    remote_endpoint: Option<Endpoint>, // 记录远程endpoint提供给connect()， 应该使用IP地址。
    metadata: SocketMetadata,
    posix_item: Arc<PosixSocketHandleItem>,
}

impl UdpSocket {
    /// 元数据的缓冲区的大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的接收缓冲区的大小 receive
    pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
    /// 默认的发送缓冲区的大小 transmiss
    pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

    /// @brief 创建一个udp的socket
    ///
    /// @param options socket的选项
    ///
    /// @return 返回创建的udp的socket
    pub fn new(options: SocketOptions) -> Self {
        let rx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_RX_BUF_SIZE],
        );
        let tx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_TX_BUF_SIZE],
        );
        let socket = udp::Socket::new(rx_buffer, tx_buffer);

        // 把socket添加到socket集合中，并得到socket的句柄
        let handle: GlobalSocketHandle =
            GlobalSocketHandle::new_smoltcp_handle(SOCKET_SET.lock_irqsave().add(socket));

        let metadata = SocketMetadata::new(
            SocketType::Udp,
            Self::DEFAULT_RX_BUF_SIZE,
            Self::DEFAULT_TX_BUF_SIZE,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        let posix_item = Arc::new(PosixSocketHandleItem::new(None));

        return Self {
            handle,
            remote_endpoint: None,
            metadata,
            posix_item,
        };
    }

    fn do_bind(&self, socket: &mut udp::Socket, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(Some(mut ip)) = endpoint {
            // 端口为0则分配随机端口
            if ip.port == 0 {
                ip.port = PORT_MANAGER.get_ephemeral_port(self.metadata.socket_type)?;
            }
            // 检测端口是否已被占用
            PORT_MANAGER.bind_port(self.metadata.socket_type, ip.port)?;

            let bind_res = if ip.addr.is_unspecified() {
                socket.bind(ip.port)
            } else {
                socket.bind(ip)
            };

            match bind_res {
                Ok(()) => return Ok(()),
                Err(_) => return Err(SystemError::EINVAL),
            }
        } else {
            return Err(SystemError::EINVAL);
        }
    }

    fn sk_setsockopt(
        &self,
        _socket: &mut udp::Socket,
        _level: SocketOptionsLevel,
        optname: PosixSocketOption,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        use PosixSocketOption::*;
        use SystemError::*;

        if optname == SO_BINDTODEVICE {
            todo!("SO_BINDTODEVICE");
        }

        match optname {
            SO_TYPE => {}
            SO_PROTOCOL => {}
            SO_DOMAIN => {}
            SO_ERROR => {
                return Err(ENOPROTOOPT);
            }
            SO_TIMESTAMP_OLD => {}
            SO_TIMESTAMP_NEW => {}
            SO_TIMESTAMPNS_OLD => {}
            
            SO_TIMESTAMPING_OLD => {}
            
            SO_RCVTIMEO_OLD => {}

            SO_SNDTIMEO_OLD => {}
            
            // if define CONFIG_NET_RX_BUSY_POLL
            SO_BUSY_POLL | SO_PREFER_BUSY_POLL | SO_BUSY_POLL_BUDGET => {
                debug!("Unsupported socket option: {:?}", optname);
                return Err(ENOPROTOOPT);
            }
            // end if
            optname => {
                debug!("Unsupported socket option: {:?}", optname);
                return Err(ENOPROTOOPT);
            }
        }
        return Ok(());
    }

    fn udp_lib_setsockopt(
        &self,
        level: SocketOptionsLevel,
        optname: usize,
        optval: &[u8],
    ) -> Result<(), SystemError> {
        use PosixSocketOption::*;

        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        let socket = socket_set_guard.get_mut::<udp::Socket>(self.handle.smoltcp_handle().unwrap());

        let so_opt_name = 
            PosixSocketOption::try_from(optname as i32)
                .map_err(|_| SystemError::ENOPROTOOPT)?;

        if level == SocketOptionsLevel::SOL_SOCKET {
            self.sk_setsockopt(socket, level, so_opt_name, optval)?;
            if so_opt_name == SO_RCVBUF || so_opt_name == SO_RCVBUFFORCE {
                todo!("SO_RCVBUF");
            }
        }

        match UdpSocketOptions::from_bits_truncate(optname as u32) {
            UdpSocketOptions::UDP_CORK => {
                todo!("UDP_CORK");
            }
            UdpSocketOptions::UDP_ENCAP => {
                match UdpEncapTypes::from_bits_truncate(optval[0]) {
                    UdpEncapTypes::ESPINUDP_NON_IKE => {
                        todo!("ESPINUDP_NON_IKE");
                    }
                    UdpEncapTypes::ESPINUDP => {
                        todo!("ESPINUDP");
                    }
                    UdpEncapTypes::L2TPINUDP => {
                        todo!("L2TPINUDP");
                    }
                    UdpEncapTypes::GTP0 => {
                        todo!("GTP0");
                    }
                    UdpEncapTypes::GTP1U => {
                        todo!("GTP1U");
                    }
                    UdpEncapTypes::RXRPC => {
                        todo!("RXRPC");
                    }
                    UdpEncapTypes::ESPINTCP => {
                        todo!("ESPINTCP");
                    }
                    UdpEncapTypes::ZERO => {}
                    _ => {
                        return Err(SystemError::ENOPROTOOPT);
                    }
                }
            }
            UdpSocketOptions::UDP_NO_CHECK6_TX => {
                todo!("UDP_NO_CHECK6_TX");
            }
            UdpSocketOptions::UDP_NO_CHECK6_RX => {
                todo!("UDP_NO_CHECK6_RX");
            }
            UdpSocketOptions::UDP_SEGMENT => {
                todo!("UDP_SEGMENT");
            }
            UdpSocketOptions::UDP_GRO => {
                todo!("UDP_GRO");
            }

            UdpSocketOptions::UDPLITE_RECV_CSCOV => {
                todo!("UDPLITE_RECV_CSCOV");
            }
            UdpSocketOptions::UDPLITE_SEND_CSCOV => {
                todo!("UDPLITE_SEND_CSCOV");
            }

            UdpSocketOptions::ZERO => {}
            _ => {
                return Err(SystemError::ENOPROTOOPT);
            }
        }
        return Ok(());
    }
}

impl Socket for UdpSocket {

    fn close(&mut self) {
        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        if let smoltcp::socket::Socket::Udp(mut sock) =
            socket_set_guard.remove(self.handle.smoltcp_handle().unwrap())
        {
            sock.close();
        }
        drop(socket_set_guard);
    }

    /// @brief 在read函数执行之前，请先bind到本地的指定端口
    fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        let socket =
            socket_set_guard.get_mut::<udp::Socket>(self.handle.smoltcp_handle().unwrap());

        if socket.can_recv() {
            if let Ok((size, metadata)) = socket.recv_slice(buf) {
                drop(socket_set_guard);
                return (Ok(size), Endpoint::Ip(Some(metadata.endpoint)));
            }
        }
        return (Err(SystemError::EAGAIN_OR_EWOULDBLOCK), Endpoint::Ip(None));
    }

    fn write(&self, buf: &[u8], to: Option<Endpoint>) -> Result<usize, SystemError> {
        let remote_endpoint: &wire::IpEndpoint = {
            if let Some(Endpoint::Ip(Some(ref endpoint))) = to {
                endpoint
            } else if let Some(Endpoint::Ip(Some(ref endpoint))) = self.remote_endpoint {
                endpoint
            } else {
                return Err(SystemError::ENOTCONN);
            }
        };

        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        let socket = socket_set_guard.get_mut::<udp::Socket>(self.handle.smoltcp_handle().unwrap());

        if socket.can_send() && socket.send_slice(buf, *remote_endpoint).is_ok() {
            return Ok(buf.len());
        }
        return Err(SystemError::ENOBUFS);
    }

    fn bind(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        let mut sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get_mut::<udp::Socket>(self.handle.smoltcp_handle().unwrap());
        // debug!("UDP Bind to {:?}", endpoint);
        return self.do_bind(socket, endpoint);
    }

    fn poll(&self) -> EPollEventType {
        let sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get::<udp::Socket>(self.handle.smoltcp_handle().unwrap());

        return SocketPollMethod::udp_poll(
            socket,
            HANDLE_MAP
                .read_irqsave()
                .get(&self.socket_handle())
                .unwrap()
                .shutdown_type(),
        );
    }

    fn connect(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(_) = endpoint {
            self.remote_endpoint = Some(endpoint);
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    fn ioctl(
        &self,
        _cmd: usize,
        _arg0: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn metadata(&self) -> SocketMetadata {
        self.metadata.clone()
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        return Box::new(self.clone());
    }

    fn setsockopt(
            &self,
            level: SocketOptionsLevel,
            optname: usize,
            optval: &[u8],
        ) -> Result<(), SystemError> {
        if level == SocketOptionsLevel::SOL_UDP || level == SocketOptionsLevel::SOL_UDPLITE || level == SocketOptionsLevel::SOL_SOCKET {
            return self.udp_lib_setsockopt(level, optname, optval);
        }
        todo!("ip_setsockopt");
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let sockets = SOCKET_SET.lock_irqsave();
        let socket = sockets.get::<udp::Socket>(self.handle.smoltcp_handle().unwrap());
        let listen_endpoint = socket.endpoint();

        if listen_endpoint.port == 0 {
            return None;
        } else {
            // 如果listen_endpoint的address是None，意味着“监听所有的地址”。
            // 这里假设所有的地址都是ipv4
            // TODO: 支持ipv6
            let result = wire::IpEndpoint::new(
                listen_endpoint
                    .addr
                    .unwrap_or(wire::IpAddress::v4(0, 0, 0, 0)),
                listen_endpoint.port,
            );
            return Some(Endpoint::Ip(Some(result)));
        }
    }

    fn peer_endpoint(&self) -> Option<Endpoint> {
        return self.remote_endpoint.clone();
    }

    fn socket_handle(&self) -> GlobalSocketHandle {
        self.handle
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
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