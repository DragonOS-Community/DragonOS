use alloc::sync::Arc;

use smoltcp;
use system_error::SystemError;

use crate::{
    libs::spinlock::SpinLock,
    net::socket::inet::common::{BoundInner, Types as InetTypes},
    process::namespace::net_namespace::NetNamespace,
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

    pub fn bind(
        self,
        local_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
    ) -> Result<BoundUdp, SystemError> {
        let inner = BoundInner::bind(self.socket, &local_endpoint.addr, netns)?;
        let bind_addr = local_endpoint.addr;
        let bind_port = if local_endpoint.port == 0 {
            let port = inner.port_manager().bind_ephemeral_port(InetTypes::Udp)?;
            log::debug!("UnboundUdp::bind: allocated ephemeral port {}", port);
            port
        } else {
            inner
                .port_manager()
                .bind_port(InetTypes::Udp, local_endpoint.port)?;
            log::debug!("UnboundUdp::bind: explicit bind to port {}", local_endpoint.port);
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
            explicitly_bound: true,
            has_preconnect_data: SpinLock::new(false),
        })
    }

    pub fn bind_ephemeral(
        self,
        remote: smoltcp::wire::IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<BoundUdp, SystemError> {
        let (inner, local_addr) = BoundInner::bind_ephemeral(self.socket, remote, netns)?;
        let bound_port = inner.port_manager().bind_ephemeral_port(InetTypes::Udp)?;
        log::debug!("UnboundUdp::bind_ephemeral: allocated ephemeral port {} for remote {:?}", bound_port, remote);

        // Bind the smoltcp socket to the local endpoint
        if local_addr.is_unspecified() {
            if inner
                .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| socket.bind(bound_port))
                .is_err()
            {
                return Err(SystemError::EINVAL);
            }
        } else if inner
            .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| {
                socket.bind(smoltcp::wire::IpEndpoint::new(local_addr, bound_port))
            })
            .is_err()
        {
            return Err(SystemError::EINVAL);
        }

        Ok(BoundUdp {
            inner,
            remote: SpinLock::new(None),
            explicitly_bound: false,
            has_preconnect_data: SpinLock::new(false),
        })
    }
}

#[derive(Debug)]
pub struct BoundUdp {
    inner: BoundInner,
    remote: SpinLock<Option<smoltcp::wire::IpEndpoint>>,
    /// True if socket was explicitly bound by user, false if implicitly bound by connect
    explicitly_bound: bool,
    /// Whether there were buffered packets at connect time - if true, allow next recv without filtering
    has_preconnect_data: SpinLock<bool>,
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

    pub fn remote_endpoint(&self) -> Result<smoltcp::wire::IpEndpoint, SystemError> {
        self.remote
            .lock()
            .as_ref()
            .cloned()
            .ok_or(SystemError::ENOTCONN)
    }

    pub fn connect(&self, remote: smoltcp::wire::IpEndpoint) {
        let local = self.endpoint();
        log::debug!("BoundUdp::connect: local={:?}, connecting to remote={:?}", local, remote);

        // Check if there are buffered packets - if so, allow next recv without filtering
        let has_buffered = self.with_socket(|socket| socket.can_recv());
        *self.has_preconnect_data.lock() = has_buffered;
        log::debug!("BoundUdp::connect: has pre-connect data = {}", has_buffered);

        self.remote.lock().replace(remote);
    }

    pub fn disconnect(&self) {
        self.remote.lock().take();
    }

    /// Returns true if this socket should be unbound on disconnect
    pub fn should_unbind_on_disconnect(&self) -> bool {
        !self.explicitly_bound
    }

    #[inline]
    pub fn try_recv(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpEndpoint), SystemError> {
        let remote = *self.remote.lock();
        let endpoint = self.endpoint();
        log::debug!("BoundUdp::try_recv: endpoint={:?}, buf_len={}, connected={:?}",
                    endpoint, buf.len(), remote);

        log::debug!("BoundUdp::try_recv: about to call with_mut_socket");
        self.with_mut_socket(|socket| {
            log::debug!("BoundUdp::try_recv: inside with_mut_socket closure");
            let can_recv = socket.can_recv();
            log::debug!("BoundUdp::try_recv: socket.can_recv()={}", can_recv);

            // If connected, filter packets by source address (except pre-connect packets)
            if let Some(expected_remote) = remote {
                // Check if we should accept pre-connect data without filtering
                let mut has_preconnect = self.has_preconnect_data.lock();
                if *has_preconnect {
                    // Clear the flag - we only allow ONE unfiltered recv
                    *has_preconnect = false;
                    drop(has_preconnect);  // Release lock before recv
                    if socket.can_recv() {
                        if let Ok((size, metadata)) = socket.recv_slice(buf) {
                            log::debug!("BoundUdp::try_recv: received pre-connect packet {} bytes from {:?}",
                                        size, metadata.endpoint);
                            return Ok((size, metadata.endpoint));
                        }
                    }
                    log::debug!("BoundUdp::try_recv: pre-connect data flag set but no data available");
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                drop(has_preconnect);  // Release lock

                // Loop to skip packets from unexpected sources
                loop {
                    if !socket.can_recv() {
                        log::debug!("BoundUdp::try_recv: connected socket, no data -> EAGAIN");
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }

                    // Peek to check source address before receiving
                    if let Ok((_size, metadata)) = socket.peek_slice(buf) {
                        log::debug!("BoundUdp::try_recv: peeked packet from {:?}, expecting {:?}",
                                    metadata.endpoint, expected_remote);
                        if metadata.endpoint == expected_remote {
                            // Source matches, receive the packet
                            if let Ok((size, metadata)) = socket.recv_slice(buf) {
                                log::debug!("BoundUdp::try_recv: received {} bytes from {:?}",
                                            size, metadata.endpoint);
                                return Ok((size, metadata.endpoint));
                            }
                        } else {
                            // Source doesn't match, discard this packet and check next
                            log::debug!("BoundUdp::try_recv: discarding packet from wrong source");
                            let _ = socket.recv_slice(buf);
                            continue;
                        }
                    }
                    log::debug!("BoundUdp::try_recv: peek failed -> EAGAIN");
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
            } else {
                // Not connected, receive from any source
                if socket.can_recv() {
                    if let Ok((size, metadata)) = socket.recv_slice(buf) {
                        log::debug!("BoundUdp::try_recv: unconnected socket received {} bytes from {:?}",
                                    size, metadata.endpoint);
                        return Ok((size, metadata.endpoint));
                    }
                }
                log::debug!("BoundUdp::try_recv: unconnected socket, no data -> EAGAIN");
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        })
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        if buf.len() == 0 {
            log::info!("UDP try_send: ZERO-LENGTH packet requested");
        }

        let connected_remote = *self.remote.lock();
        let mut remote = to.or(connected_remote).ok_or(SystemError::ENOTCONN)?;

        // Validate port - sending to port 0 is invalid
        if remote.port == 0 {
            log::warn!("UDP try_send: attempted to send to port 0");
            return Err(SystemError::EINVAL);
        }

        // Linux treats sending to 0.0.0.0 (INADDR_ANY) as sending to localhost
        // smoltcp rejects it as "Unaddressable", so we translate it here
        if remote.addr.is_unspecified() {
            remote.addr = smoltcp::wire::IpAddress::v4(127, 0, 0, 1);
            log::debug!("UDP try_send: translated unspecified address to loopback");
        }

        log::debug!(
            "UDP try_send: to={:?}, connected={:?}, final_remote={:?}, buf_len={}",
            to,
            connected_remote,
            remote,
            buf.len()
        );

        let result = self.with_mut_socket(|socket| {
            let can_send = socket.can_send();
            log::debug!("UDP try_send: can_send={}", can_send);

            if can_send {
                if buf.len() == 0 {
                    log::info!("UDP try_send: Attempting to send zero-length packet via smoltcp");
                }
                match socket.send_slice(buf, remote) {
                    Ok(_) => {
                        if buf.len() == 0 {
                            log::info!("UDP send ZERO-LENGTH packet to {:?} OK", remote);
                        } else {
                            log::debug!("UDP send {} bytes to {:?} OK", buf.len(), remote);
                        }
                        return Ok(buf.len());
                    }
                    Err(e) => {
                        if buf.len() == 0 {
                            log::error!("UDP send_slice FAILED for ZERO-LENGTH packet: {:?}", e);
                        }
                        log::warn!("UDP send_slice failed: {:?}", e);
                        return Err(SystemError::ENOBUFS);
                    }
                }
            }
            log::warn!("UDP cannot send (buffer full?)");
            return Err(SystemError::ENOBUFS);
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
