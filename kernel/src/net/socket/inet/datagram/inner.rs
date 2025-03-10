use smoltcp;
use system_error::SystemError::{self, *};

use crate::{
    libs::spinlock::SpinLock,
    net::socket::inet::common::{BoundInner, Types as InetTypes},
};

pub type SmolUdpSocket = smoltcp::socket::udp::Socket<'static>;

pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

#[derive(Debug)]
pub struct UnboundUdp {
    socket: SmolUdpSocket,
}

impl UnboundUdp {
    pub fn new() -> Self {
        let rx_buffer = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_RX_BUF_SIZE],
        );
        let tx_buffer = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_TX_BUF_SIZE],
        );
        let socket = SmolUdpSocket::new(rx_buffer, tx_buffer);

        return Self { socket };
    }

    pub fn bind(self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<BoundUdp, SystemError> {
        let inner = BoundInner::bind(self.socket, &local_endpoint.addr)?;
        let bind_addr = local_endpoint.addr;
        let bind_port = if local_endpoint.port == 0 {
            inner.port_manager().bind_ephemeral_port(InetTypes::Udp)?
        } else {
            inner
                .port_manager()
                .bind_port(InetTypes::Udp, local_endpoint.port)?;
            local_endpoint.port
        };

        if bind_addr.is_unspecified() {
            if inner
                .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| socket.bind(bind_port))
                .is_err()
            {
                return Err(SystemError::EINVAL);
            }
        } else if inner
            .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| {
                socket.bind(smoltcp::wire::IpEndpoint::new(bind_addr, bind_port))
            })
            .is_err()
        {
            return Err(SystemError::EINVAL);
        }
        Ok(BoundUdp {
            inner,
            remote: SpinLock::new(None),
        })
    }

    pub fn bind_ephemeral(self, remote: smoltcp::wire::IpAddress) -> Result<BoundUdp, SystemError> {
        // let (addr, port) = (remote.addr, remote.port);
        let (inner, address) = BoundInner::bind_ephemeral(self.socket, remote)?;
        let bound_port = inner.port_manager().bind_ephemeral_port(InetTypes::Udp)?;
        let endpoint = smoltcp::wire::IpEndpoint::new(address, bound_port);
        Ok(BoundUdp {
            inner,
            remote: SpinLock::new(Some(endpoint)),
        })
    }
}

#[derive(Debug)]
pub struct BoundUdp {
    inner: BoundInner,
    remote: SpinLock<Option<smoltcp::wire::IpEndpoint>>,
}

impl BoundUdp {
    pub fn with_mut_socket<F, T>(&self, f: F) -> T
    where
        F: FnMut(&mut SmolUdpSocket) -> T,
    {
        self.inner.with_mut(f)
    }

    pub fn with_socket<F, T>(&self, f: F) -> T
    where
        F: Fn(&SmolUdpSocket) -> T,
    {
        self.inner.with(f)
    }

    pub fn endpoint(&self) -> smoltcp::wire::IpListenEndpoint {
        self.inner
            .with::<SmolUdpSocket, _, _>(|socket| socket.endpoint())
    }

    pub fn connect(&self, remote: smoltcp::wire::IpEndpoint) {
        self.remote.lock().replace(remote);
    }

    #[inline]
    pub fn try_recv(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpEndpoint), SystemError> {
        self.with_mut_socket(|socket| {
            if socket.can_recv() {
                if let Ok((size, metadata)) = socket.recv_slice(buf) {
                    return Ok((size, metadata.endpoint));
                }
            }
            return Err(EAGAIN_OR_EWOULDBLOCK);
        })
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        let remote = to.or(*self.remote.lock()).ok_or(ENOTCONN)?;
        let result = self.with_mut_socket(|socket| {
            if socket.can_send() && socket.send_slice(buf, remote).is_ok() {
                log::debug!("send {} bytes", buf.len());
                return Ok(buf.len());
            }
            return Err(ENOBUFS);
        });
        return result;
    }

    pub fn inner(&self) -> &BoundInner {
        &self.inner
    }

    pub fn close(&self) {
        self.inner
            .iface()
            .port_manager()
            .unbind_port(InetTypes::Udp, self.endpoint().port);
        self.with_mut_socket(|socket| {
            socket.close();
        });
    }
}

// Udp Inner 负责其内部资源管理
#[derive(Debug)]
pub enum UdpInner {
    Unbound(UnboundUdp),
    Bound(BoundUdp),
}
