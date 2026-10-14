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

/// Identity belongs to the accepted datagram, never to a later binding.
pub(super) struct UdpReceived {
    pub copied: usize,
    pub source: smoltcp::wire::IpEndpoint,
    pub original_len: usize,
    pub destination: smoltcp::wire::IpEndpoint,
    pub ifindex: i32,
}

fn receive(
    socket: &mut SmolUdpSocket,
    buf: &mut [u8],
    peek: bool,
) -> Result<UdpReceived, SystemError> {
    let (payload, meta) = socket
        .peek()
        .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
    let copied = buf.len().min(payload.len());
    buf[..copied].copy_from_slice(&payload[..copied]);
    let result = UdpReceived {
        copied,
        source: meta.endpoint,
        original_len: payload.len(),
        destination: smoltcp::wire::IpEndpoint::new(
            meta.local_address
                .expect("UDP ingress records its destination"),
            meta.local_port
                .expect("UDP ingress records its destination port"),
        ),
        ifindex: meta.meta.id as i32,
    };
    if !peek {
        socket
            .recv()
            .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
    }
    Ok(result)
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
    bind_locks: BindLocks,
}

/// Linux SOCK_BINDADDR_LOCK and SOCK_BINDPORT_LOCK are independent: binding
/// port zero fixes an explicit address, not the automatically allocated port.
#[derive(Clone, Copy, Debug, Default)]
struct BindLocks {
    address: Option<smoltcp::wire::IpAddress>,
    port: bool,
}

impl UnboundUdp {
    fn recover_inner(
        inner: BoundInner,
        bind_locks: BindLocks,
        error: SystemError,
    ) -> (Self, SystemError) {
        let smoltcp::socket::Socket::Udp(socket) = inner.into_socket() else {
            unreachable!("UDP binding returned another protocol")
        };
        (Self { socket, bind_locks }, error)
    }
    pub(super) fn bound_address(&self) -> Option<smoltcp::wire::IpAddress> {
        self.bind_locks.address
    }

    pub(super) fn try_recv_with_metadata(
        &mut self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<UdpReceived, SystemError> {
        receive(&mut self.socket, buf, peek)
    }

    pub(super) fn can_recv(&self) -> bool {
        self.socket.can_recv()
    }

    pub(super) fn recv_bytes_available(&mut self) -> usize {
        self.socket.peek().map_or(0, |(payload, _)| payload.len())
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
            bind_locks: BindLocks::default(),
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
            let old_locks = original.bind_locks;
            let bind_locks = if explicitly_bound {
                BindLocks {
                    address: (!endpoint.addr.is_unspecified()).then_some(endpoint.addr),
                    port: endpoint.port != 0,
                }
            } else {
                old_locks
            };
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
            let bind_addr = bind_locks.address.unwrap_or(endpoint.addr);
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
                Err(error) => return Err(Self::recover_inner(inner, old_locks, error)),
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
                return Err(Self::recover_inner(inner, old_locks, SystemError::EINVAL));
            }
            let bound = BoundUdp {
                inner,
                connection: Mutex::new(None),
                bind_locks,
            };
            bound.update_receive_filter(bound_ifindex);
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
    bind_locks: BindLocks,
}

#[derive(Clone, Copy, Debug)]
struct UdpConnection {
    remote: smoltcp::wire::IpEndpoint,
    source: Option<smoltcp::wire::IpAddress>,
}

impl BoundUdp {
    /// Unregister transport without destroying messages already accepted.
    pub(super) fn into_unbound_after_disconnect(self) -> UnboundUdp {
        self.with_mut_socket(|socket| socket.unbind());
        let bind_locks = self.bind_locks;
        let smoltcp::socket::Socket::Udp(socket) = self.inner.into_socket() else {
            unreachable!("UDP binding returned another protocol")
        };
        UnboundUdp { socket, bind_locks }
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
        !self.bind_locks.port
    }

    pub(super) fn update_receive_filter(&self, bound_ifindex: usize) {
        let connection = *self.connection.lock();
        let filter = smoltcp::socket::udp::ReceiveFilter {
            remote: connection.map(|peer| peer.remote),
            local_address: self
                .endpoint()
                .addr
                .filter(|address| !address.is_unspecified())
                .or_else(|| connection.and_then(|peer| peer.source)),
            bound_device: core::num::NonZeroU32::new(bound_ifindex as u32),
            ingress_ifindex: self.inner.iface().nic_id() as u32,
        };
        self.with_mut_socket(|socket| socket.set_receive_filter(filter));
    }

    pub(super) fn try_recv_with_metadata(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<UdpReceived, SystemError> {
        self.with_mut_socket(|socket| receive(socket, buf, peek))
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

// Udp Inner 负责其内部资源管理
#[derive(Debug)]
pub enum UdpInner {
    Unbound(UnboundUdp),
    Bound(BoundUdp),
}
