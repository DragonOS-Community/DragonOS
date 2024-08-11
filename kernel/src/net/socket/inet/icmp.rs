


use system_error::SystemError::{self, *};
use smoltcp;
use super::{common::{BoundInner, Types}, raw::{
    DEFAULT_METADATA_BUF_SIZE, DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE
}};

pub type SmolIcmpSocket = smoltcp::socket::icmp::Socket<'static>;

#[derive(Debug)]
pub struct UnboundIcmp {
    socket: SmolIcmpSocket,
}

impl UnboundIcmp {
    pub fn new() -> Self {
        let rx_buffer = smoltcp::socket::icmp::PacketBuffer::new(
            vec![smoltcp::socket::icmp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_RX_BUF_SIZE],
        );
        let tx_buffer = smoltcp::socket::icmp::PacketBuffer::new(
            vec![smoltcp::socket::icmp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_TX_BUF_SIZE],
        );
        let socket = SmolIcmpSocket::new(rx_buffer, tx_buffer);

        return Self { socket };
    }

    pub fn ephemeral_bind(self, remote: smoltcp::wire::IpAddress) -> Result<BoundIcmp, SystemError> {
        Ok( BoundIcmp {
            inner: BoundInner::bind_ephemeral(self.socket, smoltcp::wire::IpEndpoint::new(remote, 0))?,
        })
    }

    pub fn bind(mut self, endpoint: smoltcp::wire::IpEndpoint) -> Result<BoundIcmp, SystemError> {
        if self.socket.bind(smoltcp::socket::icmp::Endpoint::Udp(
            smoltcp::wire::IpListenEndpoint::from(endpoint)
        )).is_err() {
            return Err(EINVAL);
        }
        Ok( BoundIcmp {
            inner: BoundInner::bind(self.socket, endpoint.addr)?,
        })
    }
}

#[derive(Debug)]
pub struct BoundIcmp {
    inner: BoundInner,
}

impl BoundIcmp {
    fn with_mut_socket<F, T>(&self, f: F) -> T
    where
        F: FnMut(&mut SmolIcmpSocket) -> T,
    {
        self.inner.with_mut(f)
    }

    pub fn send(&self, buf: &[u8], dst: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        if buf.len() > DEFAULT_TX_BUF_SIZE {
            return Err(EMSGSIZE);
        }
        use smoltcp::socket::icmp::SendError::*;
        self.with_mut_socket(|socket| {
            match socket.send_slice(buf, dst.addr) {
                Ok(_) => Ok(()),
                Err(Unaddressable) => Err(ECONNREFUSED),
                Err(BufferFull) => Err(ENOBUFS),
            }
        })
    }

    pub fn recv(&self, buf: &mut [u8]) -> Result<(usize, smoltcp::wire::IpAddress), SystemError> {
        use smoltcp::socket::icmp::RecvError::*;
        self.with_mut_socket(|socket| {
            match socket.recv_slice(buf) {
                Ok((size, metadata)) => Ok((size, metadata)),
                Err(Exhausted) => Err(ENOBUFS),
                Err(Truncated) => Err(EMSGSIZE),
            }
        })
    }
}

#[derive(Debug)]
pub enum IcmpInner {
    Unbound(UnboundIcmp),
    Bound(BoundIcmp),
}