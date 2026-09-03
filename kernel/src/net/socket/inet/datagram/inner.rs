use alloc::sync::{Arc, Weak};

use smoltcp;
use system_error::SystemError;

use crate::{
    libs::mutex::Mutex, net::socket::inet::common::BoundInner, net::Iface,
    process::namespace::net_namespace::NetNamespace,
};

use super::UdpSocket;

pub type SmolUdpSocket = smoltcp::socket::udp::Socket<'static>;

pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
// UDP maximum datagram size is 65507 bytes (65535 - 8 byte UDP header - 20 byte IP header)
// Set buffer sizes to accommodate this plus some overhead
pub const DEFAULT_RX_BUF_SIZE: usize = 128 * 1024; // 128 KB
pub const DEFAULT_TX_BUF_SIZE: usize = 128 * 1024; // 128 KB
                                                   // Minimum buffer size (Linux uses 256 bytes minimum)

fn output_metadata(
    remote: smoltcp::wire::IpEndpoint,
    egress_ifindex: u32,
    local_address: Option<smoltcp::wire::IpAddress>,
) -> smoltcp::socket::udp::UdpMetadata {
    let mut metadata = smoltcp::socket::udp::UdpMetadata::from(remote);
    metadata.meta.id = egress_ifindex;
    metadata.local_address = local_address;
    metadata
}

pub struct UdpBindContext {
    pub netns: Arc<NetNamespace>,
    pub socket: Weak<UdpSocket>,
    pub reuseaddr: bool,
    pub reuseport: bool,
    pub bind_id: usize,
    pub bound_ifindex: usize,
}

#[derive(Debug)]
pub struct UnboundUdp {
    socket: SmolUdpSocket,
}

impl UnboundUdp {
    pub fn new() -> Self {
        Self::new_with_buf_size(0, 0)
    }

    pub fn new_with_buf_size(rx_size: usize, tx_size: usize) -> Self {
        // Buffer sizing strategy:
        // - setsockopt(SO_RCVBUF, X) stores X
        // - getsockopt(SO_RCVBUF) returns 2*X (Linux convention)
        // - Actual buffer allocation: 2*X
        //
        // This is a straightforward 2x design that matches the getsockopt return value.
        //
        // Note: smoltcp's PacketBuffer has separate metadata_ring and payload_ring.
        // Unlike Linux where sk_buff metadata shares the same buffer space as payload,
        // smoltcp allocates them independently. This means:
        // - We allocate 2*X bytes purely for payload (no metadata overhead)
        // - This may accept more packets than Linux in some edge cases
        //
        // Differences from Linux behavior:
        // - Linux: Buffer space shared between metadata + payload, so effective payload < 2*X
        // - DragonOS: Full 2*X available for payload (metadata stored separately)

        let rx_buf_size = if rx_size > 0 {
            rx_size * 2 // Simple 2x allocation
        } else {
            DEFAULT_RX_BUF_SIZE
        };
        let tx_buf_size = if tx_size > 0 {
            tx_size * 2 // Simple 2x allocation
        } else {
            DEFAULT_TX_BUF_SIZE
        };

        // log::debug!(
        //     "new_with_buf_size: requested rx={}, tx={} -> allocating rx={}, tx={} (2x)",
        //     rx_size,
        //     tx_size,
        //     rx_buf_size,
        //     tx_buf_size
        // );

        let rx_buffer = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; rx_buf_size],
        );
        let tx_buffer = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; tx_buf_size],
        );
        let socket = SmolUdpSocket::new(rx_buffer, tx_buffer);

        return Self { socket };
    }

    pub fn bind(
        self,
        local_endpoint: smoltcp::wire::IpEndpoint,
        context: UdpBindContext,
    ) -> Result<BoundUdp, SystemError> {
        let UdpBindContext {
            netns,
            socket,
            reuseaddr,
            reuseport,
            bind_id,
            bound_ifindex,
        } = context;
        let inner = BoundInner::bind(self.socket, &local_endpoint.addr, netns.clone())?;
        let bind_addr = local_endpoint.addr;
        let bind_port_result = if local_endpoint.port == 0 {
            netns.udp_bindings().bind_ephemeral(
                socket,
                bind_addr,
                reuseaddr,
                reuseport,
                bind_id,
                bound_ifindex,
                netns.local_port_range(),
            )
        } else {
            netns
                .udp_bindings()
                .bind(
                    socket,
                    bind_addr,
                    local_endpoint.port,
                    reuseaddr,
                    reuseport,
                    bind_id,
                    bound_ifindex,
                )
                .map(|()| local_endpoint.port)
        };
        let bind_port = match bind_port_result {
            Ok(port) => port,
            Err(err) => {
                inner.release();
                return Err(err);
            }
        };

        if bind_addr.is_unspecified() {
            if inner
                .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| socket.bind(bind_port))
                .is_err()
            {
                netns.udp_bindings().unbind(bind_port, bind_id);
                inner.release();
                return Err(SystemError::EINVAL);
            }
        } else if inner
            .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| {
                socket.bind(smoltcp::wire::IpEndpoint::new(bind_addr, bind_port))
            })
            .is_err()
        {
            netns.udp_bindings().unbind(bind_port, bind_id);
            inner.release();
            return Err(SystemError::EINVAL);
        }
        Ok(BoundUdp {
            inner,
            connection: Mutex::new(None),
            explicitly_bound: true,
            has_preconnect_data: Mutex::new(false),
        })
    }

    pub fn bind_on_iface(
        self,
        iface: Arc<dyn Iface>,
        local_endpoint: smoltcp::wire::IpEndpoint,
        context: UdpBindContext,
    ) -> Result<BoundUdp, SystemError> {
        let UdpBindContext {
            netns,
            socket,
            reuseaddr,
            reuseport,
            bind_id,
            bound_ifindex,
        } = context;
        let inner = BoundInner::bind_on_iface(self.socket, iface, netns.clone())?;
        let bind_addr = local_endpoint.addr;
        let bind_port_result = if local_endpoint.port == 0 {
            netns.udp_bindings().bind_ephemeral(
                socket,
                bind_addr,
                reuseaddr,
                reuseport,
                bind_id,
                bound_ifindex,
                netns.local_port_range(),
            )
        } else {
            netns
                .udp_bindings()
                .bind(
                    socket,
                    bind_addr,
                    local_endpoint.port,
                    reuseaddr,
                    reuseport,
                    bind_id,
                    bound_ifindex,
                )
                .map(|()| local_endpoint.port)
        };
        let bind_port = match bind_port_result {
            Ok(port) => port,
            Err(err) => {
                inner.release();
                return Err(err);
            }
        };

        let endpoint = if bind_addr.is_unspecified() {
            smoltcp::wire::IpListenEndpoint::from(bind_port)
        } else {
            smoltcp::wire::IpListenEndpoint::from(smoltcp::wire::IpEndpoint::new(
                bind_addr, bind_port,
            ))
        };
        if inner
            .with_mut::<SmolUdpSocket, _, _>(|socket| socket.bind(endpoint))
            .is_err()
        {
            netns.udp_bindings().unbind(bind_port, bind_id);
            inner.release();
            return Err(SystemError::EINVAL);
        }

        Ok(BoundUdp {
            inner,
            connection: Mutex::new(None),
            explicitly_bound: true,
            has_preconnect_data: Mutex::new(false),
        })
    }

    pub fn bind_ephemeral(
        self,
        remote: smoltcp::wire::IpAddress,
        context: UdpBindContext,
    ) -> Result<BoundUdp, SystemError> {
        let UdpBindContext {
            netns,
            socket,
            reuseaddr,
            reuseport,
            bind_id,
            bound_ifindex,
        } = context;
        let (inner, local_addr) = BoundInner::bind_ephemeral(self.socket, remote, netns.clone())?;
        let bound_port = match netns.udp_bindings().bind_ephemeral(
            socket,
            local_addr,
            reuseaddr,
            reuseport,
            bind_id,
            bound_ifindex,
            netns.local_port_range(),
        ) {
            Ok(port) => port,
            Err(e) => {
                inner.release();
                return Err(e);
            }
        };
        // log::debug!(
        //     "UnboundUdp::bind_ephemeral: allocated ephemeral port {} for remote {:?}",
        //     bound_port,
        //     remote
        // );

        // Bind the smoltcp socket to the local endpoint
        if local_addr.is_unspecified() {
            if inner
                .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| socket.bind(bound_port))
                .is_err()
            {
                netns.udp_bindings().unbind(bound_port, bind_id);
                inner.release();
                return Err(SystemError::EINVAL);
            }
        } else if inner
            .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| {
                socket.bind(smoltcp::wire::IpEndpoint::new(local_addr, bound_port))
            })
            .is_err()
        {
            netns.udp_bindings().unbind(bound_port, bind_id);
            inner.release();
            return Err(SystemError::EINVAL);
        }

        Ok(BoundUdp {
            inner,
            connection: Mutex::new(None),
            explicitly_bound: false,
            has_preconnect_data: Mutex::new(false),
        })
    }

    pub fn bind_ephemeral_on_iface(
        self,
        iface: Arc<dyn Iface>,
        local_addr: smoltcp::wire::IpAddress,
        context: UdpBindContext,
    ) -> Result<BoundUdp, SystemError> {
        let UdpBindContext {
            netns,
            socket,
            reuseaddr,
            reuseport,
            bind_id,
            bound_ifindex,
        } = context;
        let inner = BoundInner::bind_on_iface(self.socket, iface, netns.clone())?;
        let bound_port = match netns.udp_bindings().bind_ephemeral(
            socket,
            local_addr,
            reuseaddr,
            reuseport,
            bind_id,
            bound_ifindex,
            netns.local_port_range(),
        ) {
            Ok(port) => port,
            Err(e) => {
                inner.release();
                return Err(e);
            }
        };

        if inner
            .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| {
                socket.bind(smoltcp::wire::IpEndpoint::new(local_addr, bound_port))
            })
            .is_err()
        {
            netns.udp_bindings().unbind(bound_port, bind_id);
            inner.release();
            return Err(SystemError::EINVAL);
        }

        Ok(BoundUdp {
            inner,
            connection: Mutex::new(None),
            explicitly_bound: false,
            has_preconnect_data: Mutex::new(false),
        })
    }
}

#[derive(Debug)]
pub struct BoundUdp {
    inner: BoundInner,
    connection: Mutex<Option<UdpConnection>>,
    /// True if socket was explicitly bound by user, false if implicitly bound by connect
    explicitly_bound: bool,
    /// Whether there were buffered packets at connect time - if true, allow next recv without filtering
    /// 这是用来模拟 Linux UDP 在应用filter前的行为。在smoltcp下，当有包到来时总是会推送到
    /// udp socket queue 中，而不是先针对connect进行filter操作。这里做workaround, 当connect是检查是否有包
    /// 在缓冲区，如果有，第一个包我们走非connect而不是connect的recv方法（即接受第一个非connect对端对应的包）
    has_preconnect_data: Mutex<bool>,
}

#[derive(Clone, Copy, Debug)]
struct UdpConnection {
    remote: smoltcp::wire::IpEndpoint,
    source: Option<smoltcp::wire::IpAddress>,
}

impl BoundUdp {
    pub fn set_explicitly_bound(&mut self, explicitly_bound: bool) {
        self.explicitly_bound = explicitly_bound;
    }
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
        (*self.connection.lock())
            .map(|connection| connection.remote)
            .ok_or(SystemError::ENOTCONN)
    }

    pub fn connect(
        &self,
        remote: smoltcp::wire::IpEndpoint,
        source: Option<smoltcp::wire::IpAddress>,
    ) {
        // let _local = self.endpoint();
        // log::debug!(
        //     "BoundUdp::connect: local={:?}, connecting to remote={:?}",
        //     _local,
        //     remote
        // );

        // Check if there are buffered packets - if so, allow next recv without filtering
        let has_buffered = self.with_socket(|socket| socket.can_recv());
        *self.has_preconnect_data.lock() = has_buffered;
        // log::debug!("BoundUdp::connect: has pre-connect data = {}", has_buffered);

        self.connection
            .lock()
            .replace(UdpConnection { remote, source });
    }

    pub fn connected_source(&self) -> Option<smoltcp::wire::IpAddress> {
        (*self.connection.lock()).and_then(|connection| connection.source)
    }

    pub fn set_preconnect_data(&self, has_data: bool) {
        *self.has_preconnect_data.lock() = has_data;
    }

    pub fn has_preconnect_data(&self) -> bool {
        *self.has_preconnect_data.lock()
    }

    pub fn take_preconnect_data(&self) -> bool {
        let mut guard = self.has_preconnect_data.lock();
        let v = *guard;
        if v {
            *guard = false;
        }
        v
    }

    pub fn disconnect(&self) {
        self.connection.lock().take();
    }

    /// Returns true if this socket should be unbound on disconnect
    pub fn should_unbind_on_disconnect(&self) -> bool {
        !self.explicitly_bound
    }

    pub fn try_recv_with_metadata(
        &self,
        buf: &mut [u8],
        peek: bool,
        bound_device_ifindex: usize,
        stack_ifindex: usize,
    ) -> Result<
        (
            usize,
            smoltcp::wire::IpEndpoint,
            usize,
            Option<smoltcp::wire::IpAddress>,
            smoltcp::phy::PacketMeta,
        ),
        SystemError,
    > {
        let connection = *self.connection.lock();
        let remote = connection.map(|connection| connection.remote);
        let connected_source = connection.and_then(|connection| connection.source);

        self.with_mut_socket(|socket| {
            let endpoint_addr = socket.endpoint().addr;
            let mut has_preconnect_guard = self.has_preconnect_data.lock();
            let has_preconnect = *has_preconnect_guard;
            if has_preconnect {
                *has_preconnect_guard = false;
            }
            drop(has_preconnect_guard);
            let should_filter = remote.is_some() && !has_preconnect;

            loop {
                if !socket.can_recv() {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                let (payload, metadata) = socket.peek().map_err(|error| match error {
                    smoltcp::socket::udp::RecvError::Exhausted => SystemError::ENOBUFS,
                    _ => SystemError::EIO,
                })?;
                let destination_mismatch = matches!(
                    (endpoint_addr, metadata.local_address),
                    (Some(bound), Some(dst))
                        if bound != dst
                            && (dst.is_multicast()
                                || dst.is_broadcast()
                                || bound.is_multicast()
                                || bound.is_broadcast())
                );
                let ingress_ifindex = usize::try_from(metadata.meta.id)
                    .ok()
                    .filter(|ifindex| *ifindex != 0)
                    .unwrap_or(stack_ifindex);
                let device_mismatch =
                    bound_device_ifindex != 0 && bound_device_ifindex != ingress_ifindex;
                let remote_mismatch =
                    should_filter && remote.is_some_and(|expected| expected != metadata.endpoint);
                let connected_destination_mismatch = should_filter
                    && connected_source.is_some_and(|source| {
                        metadata
                            .local_address
                            .is_some_and(|destination| destination != source)
                    });
                if destination_mismatch
                    || connected_destination_mismatch
                    || device_mismatch
                    || remote_mismatch
                {
                    let _ = socket.recv();
                    continue;
                }

                if buf.is_empty() {
                    return Ok((
                        0,
                        metadata.endpoint,
                        payload.len(),
                        metadata.local_address,
                        metadata.meta,
                    ));
                }
                if peek {
                    let copy_len = core::cmp::min(buf.len(), payload.len());
                    buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                    return Ok((
                        copy_len,
                        metadata.endpoint,
                        payload.len(),
                        metadata.local_address,
                        metadata.meta,
                    ));
                }

                let (recv_buf, recv_meta) = socket.recv().map_err(|_| SystemError::ENOBUFS)?;
                let length = core::cmp::min(buf.len(), recv_buf.len());
                buf[..length].copy_from_slice(&recv_buf[..length]);
                return Ok((
                    length,
                    recv_meta.endpoint,
                    recv_buf.len(),
                    recv_meta.local_address,
                    recv_meta.meta,
                ));
            }
        })
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
        egress_ifindex: u32,
        local_address: Option<smoltcp::wire::IpAddress>,
    ) -> Result<usize, SystemError> {
        let connected_remote = (*self.connection.lock()).map(|connection| connection.remote);
        let mut remote = to.or(connected_remote).ok_or(SystemError::ENOTCONN)?;

        // Validate port - sending to port 0 is invalid
        if remote.port == 0 {
            log::warn!("UDP try_send: attempted to send to port 0");
            return Err(SystemError::EINVAL);
        }

        // Linux treats an unspecified UDP destination as loopback for the same
        // address family. Keep the destination family unchanged before passing
        // it to smoltcp; mixing an IPv6 local endpoint with 127.0.0.1 would
        // violate smoltcp's IP version invariant and panic.
        if remote.addr.is_unspecified() {
            remote.addr = match remote.addr {
                smoltcp::wire::IpAddress::Ipv4(_) => smoltcp::wire::IpAddress::v4(127, 0, 0, 1),
                smoltcp::wire::IpAddress::Ipv6(_) => {
                    smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1)
                }
            };
        }

        // log::debug!(
        //     "try_send: sending {} bytes to {:?}, can_send={}",
        //     buf.len(),
        //     remote,
        //     self.with_socket(|socket| socket.can_send())
        // );

        self.with_mut_socket(|socket| {
            let max_payload = socket.payload_send_capacity();
            if buf.len() > max_payload || buf.len() > u16::MAX as usize {
                return Err(SystemError::EMSGSIZE);
            }
            if socket.can_send() {
                let metadata = output_metadata(remote, egress_ifindex, local_address);
                match socket.send_slice(buf, metadata) {
                    Ok(_) => {
                        // log::debug!("try_send: send successful");
                        Ok(buf.len())
                    }
                    Err(smoltcp::socket::udp::SendError::BufferFull) => {
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    }
                    Err(_e) => {
                        // log::debug!("try_send: send failed: {:?}", _e);
                        Err(SystemError::ENOBUFS)
                    }
                }
            } else {
                // log::debug!("try_send: can_send=false, returning EAGAIN");
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            }
        })
    }

    pub fn inner(&self) -> &BoundInner {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut BoundInner {
        &mut self.inner
    }

    pub fn close(&self) {
        self.with_mut_socket(|socket| {
            socket.close();
        });
        self.inner.release();
    }
}

#[cfg(test)]
mod tests {
    use super::output_metadata;
    use smoltcp::wire::{IpAddress, IpEndpoint};

    #[test]
    fn output_metadata_keeps_route_source_and_oif_together() {
        let remote = IpEndpoint::new(IpAddress::v4(203, 0, 113, 1), 9000);
        let source = IpAddress::v4(198, 51, 100, 2);
        let metadata = output_metadata(remote, 17, Some(source));

        assert_eq!(metadata.endpoint, remote);
        assert_eq!(metadata.local_address, Some(source));
        assert_eq!(metadata.meta.id, 17);
    }

    #[test]
    fn output_metadata_does_not_invent_a_source_for_fixed_endpoints() {
        let remote = IpEndpoint::new(IpAddress::v4(203, 0, 113, 1), 9000);
        let metadata = output_metadata(remote, 0, None);

        assert_eq!(metadata.local_address, None);
        assert_eq!(metadata.meta.id, 0);
    }
}

// Udp Inner 负责其内部资源管理
#[derive(Debug)]
pub enum UdpInner {
    Unbound(UnboundUdp),
    Bound(BoundUdp),
}
