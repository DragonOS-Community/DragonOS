use alloc::sync::{Arc, Weak};

use smoltcp;
use system_error::SystemError;

use crate::{
    libs::mutex::Mutex, net::socket::inet::common::BoundInner,
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
    pub generation: u64,
    pub bound_ifindex: usize,
}

#[derive(Debug)]
pub struct UnboundUdp {
    socket: SmolUdpSocket,
}

impl UnboundUdp {
    fn recover_inner(inner: BoundInner, error: SystemError) -> (Self, SystemError) {
        let smoltcp::socket::Socket::Udp(socket) = inner.into_socket() else {
            unreachable!("UDP binding returned another protocol")
        };
        (Self { socket }, error)
    }
    pub(super) fn with_mut_socket<T>(&mut self, f: impl FnOnce(&mut SmolUdpSocket) -> T) -> T {
        f(&mut self.socket)
    }

    /// Prepare physical ring storage in task context, before placement or
    /// socket-set locks. IPv4 ingress has its separate authoritative queue.
    pub(crate) fn try_new_with_buf_size(
        version: smoltcp::wire::IpVersion,
        rx_size: usize,
        tx_size: usize,
        memory: &crate::net::socket::buffer::SocketMemory,
    ) -> Result<Self, crate::mm::allocation::MemoryAllocationError> {
        let rx_capacity = if rx_size == 0 {
            DEFAULT_RX_BUF_SIZE
        } else {
            rx_size.checked_mul(2).ok_or(SystemError::ENOBUFS)?
        };
        let tx_capacity = if tx_size == 0 {
            DEFAULT_TX_BUF_SIZE
        } else {
            tx_size.checked_mul(2).ok_or(SystemError::ENOBUFS)?
        };
        let (rx_metadata, rx_payload) = if version == smoltcp::wire::IpVersion::Ipv4 {
            (0, 0)
        } else {
            (DEFAULT_METADATA_BUF_SIZE, rx_capacity)
        };
        let rx =
            crate::net::socket::buffer::try_owned_packet_buffer(memory, rx_metadata, rx_payload)?;
        let tx = crate::net::socket::buffer::try_owned_packet_buffer(
            memory,
            DEFAULT_METADATA_BUF_SIZE,
            tx_capacity,
        )?;
        Ok(Self {
            socket: SmolUdpSocket::new(rx, tx),
        })
    }

    /// Bind original inventory using pre-born protocol/notification vacancies.
    #[expect(
        clippy::result_large_err,
        reason = "Both function and commit closure return the original inventory on failure without allocation"
    )]
    pub(crate) fn bind_prepared(
        self,
        endpoint: smoltcp::wire::IpEndpoint,
        context: UdpBindContext,
        explicitly_bound: bool,
        prepared: &mut super::super::common::registration::PreparedIfaceSocket,
    ) -> Result<BoundUdp, (Self, SystemError)> {
        let mut original = Some(self);
        let result = prepared.with_commit(|prepared| {
            let original = original.take().expect("one protocol bind attempt");
            let inner = prepared.attach(original.socket);
            let UdpBindContext {
                netns,
                socket,
                reuseaddr,
                reuseport,
                bind_id,
                generation,
                bound_ifindex,
            } = context;
            let bind_addr = endpoint.addr;
            let port = if endpoint.port == 0 {
                netns.udp_bindings().bind_ephemeral(
                    socket.clone(),
                    bind_addr,
                    reuseaddr,
                    reuseport,
                    bind_id,
                    generation,
                    bound_ifindex,
                    netns.local_port_range(),
                )
            } else {
                netns
                    .udp_bindings()
                    .bind(
                        socket.clone(),
                        bind_addr,
                        endpoint.port,
                        reuseaddr,
                        reuseport,
                        bind_id,
                        generation,
                        bound_ifindex,
                    )
                    .map(|()| endpoint.port)
            };
            let port = match port {
                Ok(port) => port,
                Err(error) => return Err(Self::recover_inner(inner, error)),
            };
            let endpoint = if bind_addr.is_unspecified() {
                smoltcp::wire::IpListenEndpoint::from(port)
            } else {
                smoltcp::wire::IpListenEndpoint::from(smoltcp::wire::IpEndpoint::new(
                    bind_addr, port,
                ))
            };
            if inner
                .with_mut::<SmolUdpSocket, _, _>(|socket| socket.bind(endpoint))
                .is_err()
            {
                netns.udp_bindings().unbind(port, bind_id);
                return Err(Self::recover_inner(inner, SystemError::EINVAL));
            }
            let bound = BoundUdp {
                inner,
                connection: Mutex::new(None),
                explicitly_bound,
                has_preconnect_data: Mutex::new(false),
            };
            prepared.publish(socket.upgrade().expect("bind caller retains its wrapper"));
            Ok(bound)
        });
        match result {
            Ok(result) => result,
            Err(error) => Err((
                original
                    .take()
                    .expect("admission failed before consuming protocol"),
                error,
            )),
        }
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
    /// Preserve the physical inventory when the existing successful implicit
    /// disconnect resets transport state. Queue-preserving Linux disconnect
    /// semantics are tracked independently as DKC-084.
    pub(super) fn into_unbound_after_disconnect(self) -> UnboundUdp {
        self.with_mut_socket(|socket| socket.close());
        let smoltcp::socket::Socket::Udp(socket) = self.inner.into_socket() else {
            unreachable!("UDP binding returned another protocol")
        };
        UnboundUdp { socket }
    }

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
