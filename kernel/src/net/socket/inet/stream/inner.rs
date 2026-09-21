use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::filesystem::epoll::EPollEventType;
use crate::libs::mutex::Mutex;
use crate::libs::rwsem::RwSem;
use crate::net::socket::inet::common::port::TcpBindId;
use crate::net::socket::{self};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::syscall::user_buffer::UserBuffer;
use alloc::boxed::Box;
use alloc::vec::Vec;
use smoltcp;
use smoltcp::socket::tcp;
use system_error::SystemError;

use super::registration::{
    ConnectingRegistration, ConnectingRegistrationLease, ConnectingRegistrationPublisher,
};

// pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
pub const DEFAULT_RX_BUF_SIZE: usize = 128 * 1024;
pub const DEFAULT_TX_BUF_SIZE: usize = 128 * 1024;

/// Explicit closed state: no longer bound to or accessing any handle in smoltcp SocketSet.
///
/// Purpose:
/// - Semantically marks the socket as closed.
/// - Prevents concurrent update_events()/poll/notify paths from touching SocketSet
///   after the handle has been removed, which would trigger smoltcp's
///   "handle does not refer to a valid socket" panic.
#[derive(Debug, Clone, Copy)]
pub struct Closed {
    ver: smoltcp::wire::IpVersion,
}

impl Closed {
    #[inline]
    pub fn new(ver: smoltcp::wire::IpVersion) -> Self {
        Self { ver }
    }
}

fn new_smoltcp_socket_with_size(
    rx_size: usize,
    tx_size: usize,
) -> smoltcp::socket::tcp::Socket<'static> {
    let rx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; rx_size]);
    let tx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; tx_size]);
    smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer)
}

fn new_smoltcp_socket() -> smoltcp::socket::tcp::Socket<'static> {
    new_smoltcp_socket_with_size(DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE)
}

fn new_listen_smoltcp_socket<T>(
    local_endpoint: T,
    listener_id: Option<u64>,
) -> Result<smoltcp::socket::tcp::Socket<'static>, SystemError>
where
    T: Into<smoltcp::wire::IpListenEndpoint>,
{
    let mut socket = new_smoltcp_socket();
    socket.listen(local_endpoint).map_err(|e| match e {
        tcp::ListenError::InvalidState => SystemError::EINVAL, // TODO: Check is right impl
        tcp::ListenError::Unaddressable => SystemError::EADDRINUSE,
    })?;
    socket.set_listener_id(listener_id);
    Ok(socket)
}

/// Port ownership is independent of the transport slots used by a listener.
/// Moving this resource through states preserves identity; release is idempotent.
#[derive(Debug)]
pub struct TcpBinding {
    pub id: TcpBindId,
    pub local: smoltcp::wire::IpEndpoint,
    family: smoltcp::wire::IpVersion,
    netns: Arc<NetNamespace>,
    released: AtomicBool,
    /// Linux SOCK_BINDPORT_LOCK: only an explicit nonzero bind preserves the
    /// reservation when a listener is shut down.
    explicit_port: bool,
}

impl TcpBinding {
    fn new(
        id: TcpBindId,
        local: smoltcp::wire::IpEndpoint,
        family: smoltcp::wire::IpVersion,
        netns: Arc<NetNamespace>,
        explicit_port: bool,
    ) -> Self {
        Self {
            id,
            local,
            family,
            netns,
            released: AtomicBool::new(false),
            explicit_port,
        }
    }

    pub fn release(&self) {
        if !self.released.swap(true, Ordering::Relaxed) {
            self.netns
                .tcp_port_manager()
                .unbind_tcp_port(self.local.port, self.id);
        }
    }

    pub fn stop_listening(&self) {
        self.netns
            .tcp_port_manager()
            .stop_tcp_listen(self.local.port, self.id);
    }

    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::Relaxed)
    }

    pub fn shutdown_listener(&self) {
        self.stop_listening();
        if !self.explicit_port {
            // Keep the last visible endpoint for getsockname(), but release the
            // auto-assigned port as tcp_set_state(TCP_CLOSE) does on Linux.
            self.release();
        }
    }

    fn renew_if_released(
        &mut self,
        owner_uid: u32,
        reuseaddr: bool,
        reuseport: bool,
    ) -> Result<(), SystemError> {
        if !self.is_released() {
            return Ok(());
        }
        let id = TcpBindId::new();
        let port = self.netns.tcp_port_manager().bind_tcp_ephemeral_port(
            self.local.addr,
            self.family,
            reuseaddr,
            reuseport,
            owner_uid,
            id,
        )?;
        *self = Self::new(
            id,
            smoltcp::wire::IpEndpoint::new(self.local.addr, port),
            self.family,
            self.netns.clone(),
            false,
        );
        Ok(())
    }

    fn update_options(
        &self,
        reuseaddr: Option<bool>,
        reuseport: Option<bool>,
    ) -> Result<(), SystemError> {
        if self.released.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.netns.tcp_port_manager().update_tcp_options(
            self.local.port,
            self.id,
            reuseaddr,
            reuseport,
        )
    }
}

impl Drop for TcpBinding {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug)]
pub struct Bound {
    pub inner: socket::inet::BoundInner,
    // A stopped auto-bound listener retains its last local endpoint here after
    // releasing ownership. The next listen/connect renews its reservation.
    pub binding: TcpBinding,
}

#[derive(Debug)]
pub enum Init {
    Unbound(
        (
            Box<smoltcp::socket::tcp::Socket<'static>>,
            smoltcp::wire::IpVersion,
        ),
    ),
    Bound(Bound),
}

impl Init {
    pub(super) fn new(ver: smoltcp::wire::IpVersion) -> Self {
        Init::Unbound((Box::new(new_smoltcp_socket()), ver))
    }

    pub(super) fn resize_buffers(
        &mut self,
        rx_size: usize,
        tx_size: usize,
    ) -> Result<(), SystemError> {
        match self {
            Init::Unbound((socket, _)) => {
                let mut new_sock = new_smoltcp_socket_with_size(rx_size, tx_size);

                // Copy options
                new_sock.set_nagle_enabled(socket.nagle_enabled());
                new_sock.set_ack_delay(socket.ack_delay());
                new_sock.set_keep_alive(socket.keep_alive());
                new_sock.set_timeout(socket.timeout());
                new_sock.set_hop_limit(socket.hop_limit());

                **socket = new_sock;
                Ok(())
            }
            Init::Bound(Bound { inner, .. }) => {
                inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                    socket.set_send_buffer_size(tx_size);
                    socket.set_recv_buffer_size(rx_size);
                });
                Ok(())
            }
        }
    }

    pub(super) fn bind(
        self,
        local_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        reuseaddr: bool,
        reuseport: bool,
        owner_uid: u32,
    ) -> Result<Self, (Self, SystemError)> {
        match self {
            Init::Bound(Bound { inner, mut binding }) if binding.is_released() => {
                let old_iface = inner.iface().clone();
                let ver = binding.family;
                let smoltcp::socket::Socket::Tcp(socket) = inner.into_socket() else {
                    unreachable!("TCP BoundInner should contain a TCP socket");
                };
                match Init::Unbound((Box::new(socket), ver)).bind(
                    local_endpoint,
                    netns.clone(),
                    reuseaddr,
                    reuseport,
                    owner_uid,
                ) {
                    Ok(init) => Ok(init),
                    Err((Init::Unbound((socket, _)), err)) => {
                        // inet_bind preserves inet_sport on failure. A failed
                        // get_port clears the address, while an earlier address
                        // validation error preserves it (Linux 6.6 __inet_bind).
                        if err == SystemError::EADDRINUSE {
                            binding.local.addr = match ver {
                                smoltcp::wire::IpVersion::Ipv4 => {
                                    smoltcp::wire::Ipv4Address::UNSPECIFIED.into()
                                }
                                smoltcp::wire::IpVersion::Ipv6 => {
                                    smoltcp::wire::Ipv6Address::UNSPECIFIED.into()
                                }
                            };
                        }
                        // bind_on_iface only adds a socket to a known SocketSet;
                        // unlike address selection, it cannot fail.
                        let inner =
                            socket::inet::BoundInner::bind_on_iface(*socket, old_iface, netns)
                                .expect(
                                    "restoring a TCP socket on its existing interface cannot fail",
                                );
                        Err((Init::Bound(Bound { inner, binding }), err))
                    }
                    Err(_) => unreachable!("binding an unbound socket only recovers Unbound"),
                }
            }
            Init::Unbound((socket, ver)) => {
                let bound = match socket::inet::BoundInner::bind_recoverable(
                    *socket,
                    &local_endpoint.addr,
                    netns.clone(),
                ) {
                    Ok(bound) => bound,
                    Err((socket, err)) => {
                        return Err((Init::Unbound((Box::new(socket), ver)), err))
                    }
                };

                let id = TcpBindId::new();
                // Handle ephemeral port assignment (port 0)
                let bind_port = if local_endpoint.port == 0 {
                    match netns.tcp_port_manager().bind_tcp_ephemeral_port(
                        local_endpoint.addr,
                        ver,
                        reuseaddr,
                        reuseport,
                        owner_uid,
                        id,
                    ) {
                        Ok(port) => port,
                        Err(err) => {
                            let smoltcp::socket::Socket::Tcp(socket) = bound.into_socket() else {
                                unreachable!("TCP BoundInner should contain a TCP socket");
                            };
                            return Err((Init::Unbound((Box::new(socket), ver)), err));
                        }
                    }
                } else {
                    if let Err(err) = netns.tcp_port_manager().bind_tcp_port(
                        local_endpoint.port,
                        local_endpoint.addr,
                        ver,
                        reuseaddr,
                        reuseport,
                        owner_uid,
                        id,
                    ) {
                        let smoltcp::socket::Socket::Tcp(socket) = bound.into_socket() else {
                            unreachable!("TCP BoundInner should contain a TCP socket");
                        };
                        return Err((Init::Unbound((Box::new(socket), ver)), err));
                    }
                    local_endpoint.port
                };

                // Create endpoint with actual assigned port
                let final_endpoint = smoltcp::wire::IpEndpoint::new(local_endpoint.addr, bind_port);
                Ok(Init::Bound(Bound {
                    inner: bound,
                    binding: TcpBinding::new(
                        id,
                        final_endpoint,
                        ver,
                        netns,
                        local_endpoint.port != 0,
                    ),
                }))
            }
            Init::Bound(_) => {
                log::debug!("Already Bound");
                Err((self, SystemError::EINVAL))
            }
        }
    }

    pub(super) fn bind_to_ephemeral(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        owner_uid: u32,
        reuseaddr: bool,
        reuseport: bool,
    ) -> Result<Bound, (Self, SystemError)> {
        match self {
            Init::Unbound((socket, ver)) => {
                let (bound, address) = match socket::inet::BoundInner::bind_ephemeral_recoverable(
                    *socket,
                    remote_endpoint.addr,
                    netns.clone(),
                ) {
                    Ok(result) => result,
                    Err((socket, err)) => {
                        return Err((Self::Unbound((Box::new(socket), ver)), err))
                    }
                };
                let id = TcpBindId::new();
                let bound_port = match netns
                    .tcp_port_manager()
                    .bind_tcp_ephemeral_port(address, ver, reuseaddr, reuseport, owner_uid, id)
                {
                    Ok(port) => port,
                    Err(err) => {
                        let smoltcp::socket::Socket::Tcp(socket) = bound.into_socket() else {
                            unreachable!("TCP BoundInner should contain a TCP socket");
                        };
                        return Err((Self::Unbound((Box::new(socket), ver)), err));
                    }
                };
                let endpoint = smoltcp::wire::IpEndpoint::new(address, bound_port);
                Ok(Bound {
                    inner: bound,
                    binding: TcpBinding::new(id, endpoint, ver, netns, false),
                })
            }
            Init::Bound(_) => Err((self, SystemError::EINVAL)),
        }
    }

    pub(super) fn connect(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        wrapper: Weak<dyn socket::inet::InetSocket>,
        owner_uid: u32,
        reuseaddr: bool,
        reuseport: bool,
    ) -> Result<Connecting, (Self, SystemError)> {
        let Bound { inner, mut binding } = match self {
            Init::Unbound(_) => {
                self.bind_to_ephemeral(remote_endpoint, netns, owner_uid, reuseaddr, reuseport)?
            }
            Init::Bound(inner) => inner,
        };
        if let Err(err) = binding.renew_if_released(owner_uid, reuseaddr, reuseport) {
            return Err((Init::Bound(Bound { inner, binding }), err));
        }
        let local = binding.local;
        if local.addr.is_unspecified() {
            return Err((Init::Bound(Bound { inner, binding }), SystemError::EINVAL));
        }
        // Publish before taking the SocketSet lock and making the first SYN
        // visible to poll. The RAII reservation survives until Connecting is
        // replaced, while normal `bounds` registration covers the rest of the
        // socket lifetime.
        let registration = match ConnectingRegistration::try_new(inner.iface().clone(), wrapper) {
            Ok(registration) => registration,
            Err(err) => return Err((Init::Bound(Bound { inner, binding }), err)),
        };
        let result = inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket
                .connect(
                    inner.iface().smol_iface().lock().context(),
                    remote_endpoint,
                    local,
                )
                .map_err(|_| SystemError::ECONNREFUSED)
        });
        match result {
            Ok(_) => Ok(Connecting::new(
                inner,
                registration,
                binding,
                remote_endpoint,
            )),
            Err(err) => Err((Init::Bound(Bound { inner, binding }), err)),
        }
    }

    /// # `listen`
    ///
    /// Linux semantics: calling `listen()` on an unbound TCP socket auto-binds
    /// to `INADDR_ANY` (or `::`) with an ephemeral port, just like an implicit
    /// `bind(0.0.0.0:0)` before `listen()`.
    pub(super) fn listen(
        self,
        backlog: usize,
        netns: Arc<NetNamespace>,
        reuseaddr: bool,
        reuseport: bool,
        owner_uid: u32,
    ) -> Result<Listening, (Self, SystemError)> {
        // If unbound, auto-bind to INADDR_ANY:ephemeral (Linux compat).
        let bound_self = if matches!(self, Init::Unbound(_)) {
            let ver = match &self {
                Init::Unbound((_, v)) => *v,
                _ => unreachable!(),
            };
            let unspec_addr = match ver {
                smoltcp::wire::IpVersion::Ipv4 => {
                    smoltcp::wire::IpAddress::from(smoltcp::wire::Ipv4Address::UNSPECIFIED)
                }
                smoltcp::wire::IpVersion::Ipv6 => {
                    smoltcp::wire::IpAddress::from(smoltcp::wire::Ipv6Address::UNSPECIFIED)
                }
            };
            let auto_bind_ep = smoltcp::wire::IpEndpoint::new(unspec_addr, 0);
            match self.bind(auto_bind_ep, netns.clone(), reuseaddr, reuseport, owner_uid) {
                Ok(bound) => bound,
                Err((init, err)) => return Err((init, err)),
            }
        } else {
            self
        };
        let Bound { inner, mut binding } = match bound_self {
            Init::Bound(inner) => inner,
            Init::Unbound(_) => unreachable!(),
        };
        if let Err(err) = binding.renew_if_released(owner_uid, reuseaddr, reuseport) {
            return Err((Init::Bound(Bound { inner, binding }), err));
        }
        let local = binding.local;
        // IPv6 wildcard sockets are dual-stack by default. An unqualified
        // transport wildcard accepts both families; the binding still retains
        // IPv6 :: for getsockname and Linux port admission. IPv4 wildcard
        // sockets keep their family and must not accept IPv6 traffic.
        let listen_addr = match local.addr {
            smoltcp::wire::IpAddress::Ipv6(addr) if addr.is_unspecified() => {
                smoltcp::wire::IpListenEndpoint::from(local.port)
            }
            _ => smoltcp::wire::IpListenEndpoint::from(local),
        };
        if listen_addr.port == 0 {
            // Invalid port number
            return Err((Init::Bound(Bound { inner, binding }), SystemError::EINVAL));
        }
        // log::debug!("listen at {:?}, backlog {}", listen_addr, backlog);
        //
        // Linux semantics: listen(backlog=0) is valid. In practice it still allows
        // one pending connection in the accept queue (see sk_acceptq_is_full logic).
        // DragonOS uses multiple smoltcp TCP sockets to emulate accept queue slots.
        if backlog > u16::MAX as usize {
            return Err((Init::Bound(Bound { inner, binding }), SystemError::EINVAL));
        }

        // Backlog emulation:
        // - backlog==0 => emulate a single accept slot
        // - cap to avoid excessive socket allocations (FIXME: refactor backlog mechanism)
        let backlog = core::cmp::min(if backlog == 0 { 1 } else { backlog }, 8);

        if let Err(err) = netns
            .tcp_port_manager()
            .listen_tcp_port(local.port, binding.id)
        {
            return Err((Init::Bound(Bound { inner, binding }), err));
        }
        let listener_id = Some(binding.id.get());
        let mut inners = Vec::new();
        let is_any_addr = local.addr.is_unspecified();

        if let Err(err) = || -> Result<(), SystemError> {
            if is_any_addr {
                // INADDR_ANY / [::]: smoltcp uses per-interface SocketSets, so we must
                // create at least one listen socket on *every* interface; otherwise a SYN
                // arriving on an interface without a listen socket gets no response (RST
                // or silent drop depending on smoltcp version).
                //
                // Strategy: place ≥1 listen socket on each interface. Any remaining
                // backlog slots go to the primary interface.
                let device_list = netns.device_list();
                for (_, iface) in device_list.iter() {
                    if alloc::sync::Arc::ptr_eq(iface, inner.iface()) {
                        continue; // primary inner already covers this iface
                    }
                    let new_listen = socket::inet::BoundInner::bind_on_iface(
                        new_listen_smoltcp_socket(listen_addr, listener_id)?,
                        iface.clone(),
                        inner.netns(),
                    )?;
                    inners.push(new_listen);
                }
                // Fill remaining backlog slots on the primary interface.
                let remaining = backlog.saturating_sub(1 + inners.len());
                for _ in 0..remaining {
                    let new_listen = socket::inet::BoundInner::bind_on_iface(
                        new_listen_smoltcp_socket(listen_addr, listener_id)?,
                        inner.iface().clone(),
                        inner.netns(),
                    )?;
                    inners.push(new_listen);
                }
            } else {
                // Specific address: all backlog sockets go to the same interface.
                let additional_sockets = backlog.saturating_sub(1);
                for _ in 0..additional_sockets {
                    let new_listen = socket::inet::BoundInner::bind(
                        new_listen_smoltcp_socket(listen_addr, listener_id)?,
                        listen_addr
                            .addr
                            .as_ref()
                            .unwrap_or(&smoltcp::wire::IpAddress::from(
                                smoltcp::wire::Ipv4Address::UNSPECIFIED,
                            )),
                        inner.netns(),
                    )?;
                    inners.push(new_listen);
                }
            }
            Ok(())
        }() {
            for slot in &inners {
                slot.release();
            }
            binding.stop_listening();
            return Err((Init::Bound(Bound { inner, binding }), err));
        }

        if let Err(err) = inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.listen(listen_addr).map_err(|err| match err {
                tcp::ListenError::InvalidState => SystemError::EINVAL,
                tcp::ListenError::Unaddressable => SystemError::EINVAL,
            })?;
            socket.set_listener_id(listener_id);
            Ok::<(), SystemError>(())
        }) {
            for slot in &inners {
                slot.release();
            }
            binding.stop_listening();
            return Err((Init::Bound(Bound { inner, binding }), err));
        }

        inners.push(inner);
        return Ok(Listening {
            inners,
            connect: AtomicUsize::new(0),
            listen_addr,
            binding,
            listener_id,
        });
    }

    pub(super) fn close(self) -> Closed {
        match self {
            Init::Unbound((_, version)) => Closed::new(version),
            Init::Bound(Bound { inner, binding }) => {
                binding.release();
                let version = binding.family;
                let _ = inner.into_socket();
                Closed::new(version)
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
enum ConnectResult {
    Connected,
    #[default]
    Connecting,
    Refused,
    RefusedConsumed,
    ShutdownReset,
    ShutdownResetConsumed,
}

#[derive(Debug)]
pub struct Connecting {
    binding: TcpBinding,
    inner: socket::inet::BoundInner,
    registration: ConnectingRegistration,
    result: RwSem<ConnectResult>,
    /// Track if the connection was ever in ESTABLISHED state.
    /// This is needed because for loopback, SYN+ACK and RST can be processed in the same poll,
    /// so we might miss the ESTABLISHED state. If we were ever established, receiving RST
    /// should not be treated as "connection refused" but as "connection reset".
    was_established: AtomicBool,
    local: smoltcp::wire::IpEndpoint,
    remote: smoltcp::wire::IpEndpoint,
}

impl Connecting {
    fn new(
        inner: socket::inet::BoundInner,
        registration: ConnectingRegistration,
        binding: TcpBinding,
        remote: smoltcp::wire::IpEndpoint,
    ) -> Self {
        let local = binding.local;
        Connecting {
            inner,
            registration,
            binding,
            result: RwSem::new(ConnectResult::Connecting),
            was_established: AtomicBool::new(false),
            local,
            remote,
        }
    }

    pub fn with_mut<R, F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>) -> R>(
        &self,
        f: F,
    ) -> R {
        self.inner.with_mut(f)
    }

    pub fn with<R, F: Fn(&smoltcp::socket::tcp::Socket<'static>) -> R>(&self, f: F) -> R {
        self.inner.with(f)
    }

    pub fn iface(&self) -> &Arc<dyn crate::net::Iface> {
        self.inner.iface()
    }

    pub(super) fn registration_publisher(&self) -> ConnectingRegistrationPublisher {
        self.registration.publisher()
    }

    pub fn into_result(mut self) -> (Inner, Result<(), SystemError>) {
        let result = *self.result.read();
        match result {
            ConnectResult::Connecting => (
                Inner::Connecting(self),
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            ),
            ConnectResult::Connected => {
                let registration = self.registration.retain();
                (
                    Inner::Established(Established::new_with_connecting_registration(
                        self.inner,
                        Some(self.binding),
                        registration,
                    )),
                    Ok(()),
                )
            }
            ConnectResult::Refused
            | ConnectResult::RefusedConsumed
            | ConnectResult::ShutdownReset
            | ConnectResult::ShutdownResetConsumed => {
                self.binding.release();
                let socket = self.inner.into_socket();
                let socket = match socket {
                    smoltcp::socket::Socket::Tcp(s) => s,
                    _ => panic!("Connecting socket is not TCP"),
                };
                let ver = self.binding.family;
                let err = match result {
                    ConnectResult::ShutdownReset | ConnectResult::ShutdownResetConsumed => {
                        SystemError::ECONNRESET
                    }
                    _ => SystemError::ECONNREFUSED,
                };
                (
                    Inner::Init(Init::Unbound((Box::new(socket), ver))),
                    Err(err),
                )
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(*self.result.read(), ConnectResult::Connected)
    }

    pub fn is_transport_established(&self) -> bool {
        self.with(|socket| {
            matches!(
                socket.state(),
                tcp::State::Established | tcp::State::CloseWait
            )
        })
    }

    /// Transmutes the Connecting state to Established state.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it forces a state transition without verifying
    /// that the underlying socket is actually in the ESTABLISHED state.
    /// The caller must ensure that the socket handshake has completed successfully.
    pub unsafe fn into_established(mut self) -> Established {
        // Every live Connecting -> Established transition transfers the
        // existing iface notification registration. Callers that are closing
        // the socket have already unregistered it before reaching here.
        let registration = self.registration.retain();
        Established::new_with_connecting_registration(self.inner, Some(self.binding), registration)
    }

    /// Converts a Connecting socket after its iface registration was removed
    /// by the close path. A delayed publisher observes cancellation and rolls
    /// back any registration racing with close.
    pub unsafe fn into_established_after_unbind(self) -> Established {
        self.registration.cancel();
        Established::new(self.inner, Some(self.binding))
    }

    /// Returns `true` when `conn_result` becomes ready, which indicates that the caller should
    /// invoke the `into_result()` method as soon as possible.
    ///
    /// Since `into_result()` needs to be called only once, this method will return `true`
    /// _exactly_ once. The caller is responsible for not missing this event.
    #[must_use]
    pub(super) fn update_io_events(&self, pollee: &core::sync::atomic::AtomicUsize) -> bool {
        self.inner
            .with_mut(|socket: &mut smoltcp::socket::tcp::Socket| {
                let mut result = self.result.write();
                let state = socket.state();

                // Track if we ever reach ESTABLISHED state
                if matches!(state, tcp::State::Established | tcp::State::CloseWait) {
                    self.was_established
                        .store(true, core::sync::atomic::Ordering::Relaxed);
                }

                let was_established = self
                    .was_established
                    .load(core::sync::atomic::Ordering::Relaxed);

                // Heuristic: if socket has valid remote endpoint AND local endpoint in CLOSED state,
                // it likely completed the handshake before receiving RST. This helps detect the case
                // where SYN+ACK and RST are processed in the same poll() call for loopback.
                let endpoints_valid =
                    socket.local_endpoint().is_some() && socket.remote_endpoint().is_some();
                let likely_was_established =
                    was_established || (matches!(state, tcp::State::Closed) && endpoints_valid);

                // Only update result if not already final
                if !matches!(
                    *result,
                    ConnectResult::Refused
                        | ConnectResult::Connected
                        | ConnectResult::RefusedConsumed
                        | ConnectResult::ShutdownReset
                        | ConnectResult::ShutdownResetConsumed
                ) {
                    if matches!(state, tcp::State::Established | tcp::State::CloseWait) {
                        // log::debug!(
                        //     "tcp connected: state={:?} local={:?} remote={:?}",
                        //     state,
                        //     socket.local_endpoint(),
                        //     socket.remote_endpoint()
                        // );
                        *result = ConnectResult::Connected;
                    } else if socket.is_open() {
                        *result = ConnectResult::Connecting;
                    } else {
                        // Socket is closed. Determine if it was ever established.
                        if likely_was_established {
                            // Connection was established, then closed (e.g., received RST after handshake)
                            log::debug!(
                                "tcp connection reset: state={:?} local={:?} remote={:?}",
                                state,
                                socket.local_endpoint(),
                                socket.remote_endpoint()
                            );
                            *result = ConnectResult::Connected;
                        } else {
                            // Connection was never established (refused)
                            // log::debug!(
                            //     "tcp connect refused: state={:?} local={:?} remote={:?}",
                            //     state,
                            //     socket.local_endpoint(),
                            //     socket.remote_endpoint()
                            // );
                            *result = ConnectResult::Refused;
                        }
                    }
                }

                // Update pollee based on current result
                // CRITICAL: For Connecting state, we only set POLLOUT | POLLWRNORM when connect
                // completes (success or failure). We do NOT set POLLHUP/POLLRDHUP here!
                // Those events will be set by Established::update_io_events() after the state
                // transition, which correctly reflects the actual socket state.

                match *result {
                    ConnectResult::Connected => {
                        // Connection attempt completed successfully
                        // Set only POLLOUT | POLLWRNORM to indicate connect() completed.
                        // Clear all other flags - Established::update_io_events() will set
                        // the correct flags after state transition.
                        pollee.fetch_or(
                            (EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM).bits()
                                as usize,
                            Ordering::Relaxed,
                        );
                        // Clear error/hangup bits - they should not be set while in Connecting state
                        pollee.fetch_and(
                            !(EPollEventType::EPOLLIN
                                | EPollEventType::EPOLLERR
                                | EPollEventType::EPOLLHUP
                                | EPollEventType::EPOLLRDHUP
                                | EPollEventType::EPOLLRDNORM)
                                .bits() as usize,
                            Ordering::Relaxed,
                        );
                    }
                    ConnectResult::Refused
                    | ConnectResult::RefusedConsumed
                    | ConnectResult::ShutdownReset
                    | ConnectResult::ShutdownResetConsumed => {
                        // Connection attempt refused (or reset during handshake).
                        // This is equivalent to a closed socket with error.
                        // Should be readable, writable, and have HUP/ERR set.

                        let mut events_to_set = EPollEventType::EPOLLIN
                            | EPollEventType::EPOLLRDNORM
                            | EPollEventType::EPOLLOUT
                            | EPollEventType::EPOLLWRNORM
                            | EPollEventType::EPOLLHUP
                            | EPollEventType::EPOLLRDHUP;

                        // If error not consumed yet, set EPOLLERR
                        if matches!(
                            *result,
                            ConnectResult::Refused | ConnectResult::ShutdownReset
                        ) {
                            events_to_set |= EPollEventType::EPOLLERR;
                        }

                        pollee.fetch_or(events_to_set.bits() as usize, Ordering::Relaxed);

                        // If error IS consumed, clear EPOLLERR (if it was set previously)
                        if matches!(
                            *result,
                            ConnectResult::RefusedConsumed | ConnectResult::ShutdownResetConsumed
                        ) {
                            pollee.fetch_and(
                                !(EPollEventType::EPOLLERR).bits() as usize,
                                Ordering::Relaxed,
                            );
                        }
                    }
                    ConnectResult::Connecting => {
                        // Still connecting - clear all events
                        pollee.fetch_and(
                            !(EPollEventType::EPOLLIN
                                | EPollEventType::EPOLLOUT
                                | EPollEventType::EPOLLERR
                                | EPollEventType::EPOLLHUP
                                | EPollEventType::EPOLLRDHUP
                                | EPollEventType::EPOLLRDNORM
                                | EPollEventType::EPOLLWRNORM)
                                .bits() as usize,
                            Ordering::Relaxed,
                        );
                    }
                }

                matches!(
                    *result,
                    ConnectResult::Refused
                        | ConnectResult::Connected
                        | ConnectResult::RefusedConsumed
                        | ConnectResult::ShutdownReset
                        | ConnectResult::ShutdownResetConsumed
                )
            })
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        self.local
    }

    pub fn get_peer_name(&self) -> smoltcp::wire::IpEndpoint {
        self.remote
    }

    pub fn failure_reason(&self) -> Option<SystemError> {
        match *self.result.read() {
            ConnectResult::Refused => Some(SystemError::ECONNREFUSED),
            ConnectResult::ShutdownReset => Some(SystemError::ECONNRESET),
            _ => None,
        }
    }

    pub fn consume_error(&self) {
        let mut guard = self.result.write();
        match *guard {
            ConnectResult::Refused => *guard = ConnectResult::RefusedConsumed,
            ConnectResult::ShutdownReset => *guard = ConnectResult::ShutdownResetConsumed,
            _ => {}
        }
    }

    pub fn is_refused_consumed(&self) -> bool {
        matches!(
            *self.result.read(),
            ConnectResult::RefusedConsumed | ConnectResult::ShutdownResetConsumed
        )
    }

    pub fn set_shutdown_reset(&self) {
        *self.result.write() = ConnectResult::ShutdownReset;
    }
}

#[derive(Debug)]
pub struct Listening {
    pub inners: Vec<socket::inet::BoundInner>,
    connect: AtomicUsize,
    listen_addr: smoltcp::wire::IpListenEndpoint,
    pub binding: TcpBinding,
    listener_id: Option<u64>,
}

impl Listening {
    pub fn accept(&mut self) -> Result<(Established, smoltcp::wire::IpEndpoint), SystemError> {
        let connected = self
            .inners
            .get_mut(self.connect.load(core::sync::atomic::Ordering::Relaxed))
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

        // Allocate the replacement before locking SocketSet, but publish it only
        // after a final state/tuple check under the same lock as the handoff.
        let replacement = new_listen_smoltcp_socket(self.listen_addr, self.listener_id)?;
        let (accepted, local, remote) = connected.accept_tcp(replacement)?;
        // Use the endpoint snapshot from the handoff: an RST after unlocking may
        // already have cleared the transport tuple before this object is built.
        Ok((
            Established {
                inner: accepted,
                local,
                peer: remote,
                binding: None,
                connecting_registration: None,
            },
            remote,
        ))
    }

    pub fn update_io_events(&self, pollee: &AtomicUsize) {
        // Linux 6.6: tcp_poll() 对 TCP_LISTEN 直接早返回 inet_csk_listen_poll()，其返回值
        // 只可能是 EPOLLIN | EPOLLRDNORM（accept 队列非空）或 0 —— LISTEN 套接字的就绪掩码
        // 每次都是重算的，永远不会出现 EPOLLHUP / EPOLLRDHUP / EPOLLERR。
        //
        // DragonOS 的 pollee 是累积式（置位/清除）的，所以必须显式清掉历史残留：
        // 套接字在 bind() 后就注册进了 iface 的通知链，而那时它还处于 Init 状态，
        // Init 分支按 Linux 非 LISTEN（TCP_CLOSE）的语义打过 EPOLLHUP。
        // 不清理的话，这个 HUP 会粘滞整个 listen 生命周期，使
        // poll(listen_fd, POLLIN, 0) 恒返回就绪（gVisor PollAroundAccept）。
        pollee.fetch_and(
            !(EPollEventType::EPOLLHUP | EPollEventType::EPOLLRDHUP | EPollEventType::EPOLLERR)
                .bits() as usize,
            core::sync::atomic::Ordering::Relaxed,
        );

        // A failed pending connection frees its slot. Re-arm it with the same
        // logical identity; a Closed transport cannot represent a full listener.
        for inner in &self.inners {
            inner.with_mut::<tcp::Socket, _, _>(|socket| {
                if socket.state() == tcp::State::Closed && socket.listen(self.listen_addr).is_ok() {
                    socket.set_listener_id(self.listener_id);
                }
            });
        }
        let position = self.inners.iter().position(|inner| {
            inner.with::<tcp::Socket, _, _>(|socket| {
                matches!(
                    socket.state(),
                    tcp::State::Established | tcp::State::CloseWait
                )
            })
        });

        let events = if let Some(position) = position {
            self.connect
                .store(position, core::sync::atomic::Ordering::Relaxed);
            EPollEventType::EPOLL_LISTEN_CAN_ACCEPT.bits() as usize
        } else {
            0
        };
        // Listening readiness is determined solely by completed accept slots.
        // In particular, do not inherit Init's HUP or a previous connection's
        // writable/error flags across bind/listen and shutdown/listen.
        pollee.store(events, core::sync::atomic::Ordering::Relaxed);
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        self.binding.local
    }

    pub fn close(&self) {
        for inner in &self.inners {
            inner.with_mut::<tcp::Socket, _, _>(|socket| {
                socket.set_listener_id(None);
                socket.abort();
            });
        }
        self.binding.release();
    }

    pub fn release(&mut self) {
        // log::debug!("Release Listening Socket");
        for inner in self.inners.iter() {
            inner.release();
        }
    }
}

#[derive(Debug)]
pub struct Established {
    inner: socket::inet::BoundInner,
    local: smoltcp::wire::IpEndpoint,
    peer: smoltcp::wire::IpEndpoint,
    binding: Option<TcpBinding>,
    connecting_registration: Option<ConnectingRegistrationLease>,
}

impl Established {
    pub fn new(inner: socket::inet::BoundInner, binding: Option<TcpBinding>) -> Self {
        let local = inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.local_endpoint())
            .unwrap_or(smoltcp::wire::IpEndpoint::new(
                smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                0,
            ));
        let peer = inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.remote_endpoint())
            .unwrap_or(smoltcp::wire::IpEndpoint::new(
                smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                0,
            ));
        Self {
            inner,
            local,
            peer,
            binding,
            connecting_registration: None,
        }
    }

    fn new_with_connecting_registration(
        inner: socket::inet::BoundInner,
        binding: Option<TcpBinding>,
        registration: ConnectingRegistrationLease,
    ) -> Self {
        let mut established = Self::new(inner, binding);
        established.connecting_registration = Some(registration);
        established
    }

    pub(super) fn cancel_connecting_registration(&self) {
        if let Some(registration) = &self.connecting_registration {
            registration.cancel();
        }
    }

    pub fn with_mut<R, F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>) -> R>(
        &self,
        f: F,
    ) -> R {
        self.inner.with_mut(f)
    }

    pub fn with<R, F: Fn(&smoltcp::socket::tcp::Socket<'static>) -> R>(&self, f: F) -> R {
        self.inner.with(f)
    }

    pub fn iface(&self) -> &Arc<dyn crate::driver::net::Iface> {
        self.inner.iface()
    }

    pub fn handle(&self) -> smoltcp::iface::SocketHandle {
        self.inner.handle()
    }

    pub fn release_binding(&self) {
        if let Some(binding) = &self.binding {
            binding.release();
        }
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        // smoltcp may clear endpoints in TIME_WAIT/CLOSED; keep a cached copy.
        self.inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.local_endpoint())
            .unwrap_or(self.local)
    }

    pub fn get_peer_name(&self) -> smoltcp::wire::IpEndpoint {
        // smoltcp may clear endpoints in TIME_WAIT/CLOSED; keep a cached copy.
        self.inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.remote_endpoint())
            .unwrap_or(self.peer)
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                if socket.can_send() {
                    socket
                        .send_slice(buf)
                        .map_err(|_| SystemError::ECONNABORTED)
                } else {
                    match socket.state() {
                        smoltcp::socket::tcp::State::Closed => Err(SystemError::ECONNRESET),
                        smoltcp::socket::tcp::State::TimeWait
                        | smoltcp::socket::tcp::State::Closing
                        | smoltcp::socket::tcp::State::LastAck => Err(SystemError::EPIPE),
                        _ => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                    }
                }
            })
    }

    pub fn update_io_events(&self, pollee: &AtomicUsize) {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                let state = socket.state();

                // Check if socket is still open and in a "normal" connected state
                let is_connected = matches!(
                    state,
                    smoltcp::socket::tcp::State::Established
                        | smoltcp::socket::tcp::State::SynReceived
                );

                // FIN received states: peer has closed their side
                let fin_received = matches!(
                    state,
                    smoltcp::socket::tcp::State::CloseWait
                        | smoltcp::socket::tcp::State::LastAck
                        | smoltcp::socket::tcp::State::Closing
                        | smoltcp::socket::tcp::State::TimeWait
                        | smoltcp::socket::tcp::State::Closed
                );

                // Socket closed (no more I/O possible)
                let is_closed = matches!(
                    state,
                    smoltcp::socket::tcp::State::TimeWait | smoltcp::socket::tcp::State::Closed
                );

                if socket.can_send() {
                    pollee.fetch_or(
                        (EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM).bits() as usize,
                        Ordering::Relaxed,
                    );
                } else {
                    pollee.fetch_and(
                        !(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM).bits() as usize,
                        Ordering::Relaxed,
                    );
                }

                // EPOLLIN should be set if there is data to read OR if the socket has received FIN (EOF).
                if socket.can_recv() || fin_received {
                    pollee.fetch_or(
                        (EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize,
                        Ordering::Relaxed,
                    );
                } else {
                    pollee.fetch_and(
                        !(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize,
                        Ordering::Relaxed,
                    );
                }

                // Handle EPOLLHUP, EPOLLRDHUP, EPOLLERR based on socket state
                // CRITICAL: When socket is still connected, clear these flags!
                // This fixes the issue where Connecting state might have set these flags
                // before transitioning to Established.
                if is_connected {
                    // Socket is open and connected - clear all error/hangup flags
                    pollee.fetch_and(
                        !(EPollEventType::EPOLLHUP
                            | EPollEventType::EPOLLRDHUP
                            | EPollEventType::EPOLLERR)
                            .bits() as usize,
                        Ordering::Relaxed,
                    );
                } else if fin_received && !is_closed {
                    // Peer sent FIN but socket not fully closed yet (CloseWait, LastAck, Closing)
                    // Set EPOLLRDHUP to indicate peer shutdown for reading
                    pollee.fetch_or(
                        EPollEventType::EPOLLRDHUP.bits() as usize,
                        Ordering::Relaxed,
                    );
                    // Clear EPOLLHUP (full hangup) and EPOLLERR (no error)
                    pollee.fetch_and(
                        !(EPollEventType::EPOLLHUP | EPollEventType::EPOLLERR).bits() as usize,
                        Ordering::Relaxed,
                    );
                } else if is_closed {
                    // Socket fully closed - set both EPOLLHUP and EPOLLRDHUP
                    pollee.fetch_or(
                        (EPollEventType::EPOLLHUP | EPollEventType::EPOLLRDHUP).bits() as usize,
                        Ordering::Relaxed,
                    );
                    // Clear EPOLLERR - closed is not an error condition
                    pollee.fetch_and(
                        !(EPollEventType::EPOLLERR).bits() as usize,
                        Ordering::Relaxed,
                    );
                }
            })
    }
}

/// Linux-compatible TCP "self-connect" (connect to the same local addr:port on the same socket).
///
/// Linux allows this with a single socket FD, and bytes written to the socket are readable
/// back from the same socket. smoltcp's TCP socket cannot model this with a single instance,
/// because a TCP endpoint should not receive its own outbound segments.
///
/// We implement the user-visible semantics by internally queueing sent bytes into a local
/// receive queue, and driving readiness/EOF based on shutdown state.
#[derive(Debug)]
pub struct SelfConnected {
    binding: TcpBinding,
    inner: socket::inet::BoundInner,
    local: smoltcp::wire::IpEndpoint,
    state: Mutex<SelfConnectedState>,
}

#[derive(Debug)]
struct SelfConnectedState {
    /// Effective receive capacity for the loopback queue (bytes).
    rx_cap: usize,
    buf: VecDeque<u8>,
    /// Ordered EOF marker for the self-connected byte stream.
    ///
    /// This must be protected by the same lock as `buf`: a concurrent
    /// `send()` and `shutdown(SHUT_WR)` must linearize as either
    /// data-before-FIN or FIN-before-send, never as an independently visible EOF.
    send_shutdown: bool,
}

impl SelfConnected {
    pub fn new(bound: Bound, rx_cap: usize) -> Self {
        Self {
            inner: bound.inner,
            local: bound.binding.local,
            binding: bound.binding,
            state: Mutex::new(SelfConnectedState {
                rx_cap,
                buf: VecDeque::new(),
                send_shutdown: false,
            }),
        }
    }

    #[allow(dead_code)]
    pub fn iface(&self) -> &Arc<dyn crate::driver::net::Iface> {
        self.inner.iface()
    }

    #[allow(dead_code)]
    pub fn handle(&self) -> smoltcp::iface::SocketHandle {
        self.inner.handle()
    }

    #[inline]
    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        self.local
    }

    #[inline]
    pub fn get_peer_name(&self) -> smoltcp::wire::IpEndpoint {
        self.local
    }

    #[inline]
    pub fn recv_queue(&self) -> usize {
        self.state.lock().buf.len()
    }

    pub fn discard_all(&self) {
        self.state.lock().buf.clear();
    }

    pub fn set_recv_buffer_size(&self, rx_size: usize) {
        self.state.lock().rx_cap = rx_size;
    }

    pub fn recv_capacity(&self) -> usize {
        self.state.lock().rx_cap
    }

    pub fn send_capacity(&self) -> usize {
        // For self-connect, use the same capacity for "send" as the local receive queue.
        self.state.lock().rx_cap
    }

    pub fn send_slice(&self, data: &[u8], send_shutdown: bool) -> Result<usize, SystemError> {
        if send_shutdown {
            return Err(SystemError::EPIPE);
        }
        if data.is_empty() {
            return Ok(0);
        }

        let mut state = self.state.lock();
        if state.send_shutdown {
            return Err(SystemError::EPIPE);
        }

        let free = state.rx_cap.saturating_sub(state.buf.len());
        if free == 0 {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let n = core::cmp::min(free, data.len());
        state.buf.extend(data[..n].iter().copied());
        Ok(n)
    }

    /// Set the send_shutdown flag (called when SHUT_WR is performed).
    pub fn set_send_shutdown(&self) {
        self.state.lock().send_shutdown = true;
    }

    /// Check if send_shutdown flag is set.
    #[inline]
    #[allow(dead_code)]
    pub fn is_send_shutdown(&self) -> bool {
        self.state.lock().send_shutdown
    }

    pub fn recv_into(&self, out: &mut [u8], peek: bool, trunc: bool) -> Result<usize, SystemError> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self.state.lock();
        if state.buf.is_empty() {
            // EOF after SHUT_WR once all queued data is drained.
            if state.send_shutdown {
                return Ok(0);
            }
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let n = core::cmp::min(out.len(), state.buf.len());
        if !trunc {
            for (i, b) in state.buf.iter().take(n).enumerate() {
                out[i] = *b;
            }
        }

        if !peek {
            for _ in 0..n {
                let _ = state.buf.pop_front();
            }
        }
        Ok(n)
    }

    pub fn recv_to_user(
        &self,
        out: &mut UserBuffer<'_>,
        offset: usize,
        max_len: usize,
    ) -> Result<usize, SystemError> {
        if offset > out.len() {
            return Err(SystemError::EINVAL);
        }
        let available = core::cmp::min(out.len() - offset, max_len);
        if available == 0 {
            return Ok(0);
        }

        let mut state = self.state.lock();
        if state.buf.is_empty() {
            if state.send_shutdown {
                return Ok(0);
            }
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let n = core::cmp::min(available, state.buf.len());
        let mut tmp = Vec::with_capacity(n);
        tmp.extend(state.buf.iter().take(n).copied());

        match out.write_to_user(offset, &tmp) {
            Ok(_) => {
                for _ in 0..n {
                    let _ = state.buf.pop_front();
                }
                Ok(n)
            }
            Err(SystemError::EFAULT) => Err(SystemError::EFAULT),
            Err(e) => Err(e),
        }
    }

    /// 重算 self-connect 套接字的就绪掩码。
    ///
    /// `recv_shutdown` 是外层 `TcpSocket` 记录的 `SHUT_RD` 状态（内层只跟踪发送侧）。
    ///
    /// Linux 6.6 `tcp_poll()` 的挂断语义：
    /// ```c
    ///     if (shutdown == SHUTDOWN_MASK || state == TCP_CLOSE)
    ///         mask |= EPOLLHUP;
    ///     if (shutdown & RCV_SHUTDOWN)
    ///         mask |= EPOLLIN | EPOLLRDNORM | EPOLLRDHUP;
    /// ```
    /// self-connect 发出的 FIN 会回到自己（`tcp_fin()` 收到 FIN 时置 `RCV_SHUTDOWN`），
    /// 因此 `SHUT_WR` 之后本端的 `sk_shutdown` 已经是 `SHUTDOWN_MASK`：读侧同时进入 EOF，
    /// `poll()` 应报 `EPOLLRDHUP | EPOLLHUP`（Linux 实测为 `IN|OUT|HUP|RDHUP`）。
    /// 只有 `SHUT_RD` 时是半关闭，只报 `EPOLLRDHUP`，不得报 `EPOLLHUP`。
    /// 连接仍完全建立时必须清掉历史残留：套接字在 `bind()` 后即被 iface 通知，
    /// `Init` 分支会打上 `EPOLLHUP`，不清理会让 `poll`/`epoll` 永久误报挂断。
    /// `EPOLLERR` 在 self-connect 上没有产生路径，只清不置。
    pub fn update_io_events(&self, pollee: &AtomicUsize, recv_shutdown: bool) {
        let state = self.state.lock();
        let send_shutdown = state.send_shutdown;
        let writable = !send_shutdown && state.buf.len() < state.rx_cap;
        let readable = !state.buf.is_empty() || send_shutdown;
        drop(state);

        let read_shutdown = send_shutdown || recv_shutdown;

        let hangup_bits =
            (EPollEventType::EPOLLHUP | EPollEventType::EPOLLRDHUP | EPollEventType::EPOLLERR)
                .bits() as usize;
        let mut rebuilt = 0usize;
        if read_shutdown {
            rebuilt |= EPollEventType::EPOLLRDHUP.bits() as usize;
        }
        if send_shutdown {
            // 自身 FIN 回环到读侧，`SHUT_WR` 之后 sk_shutdown 已是 SHUTDOWN_MASK。
            rebuilt |= EPollEventType::EPOLLHUP.bits() as usize;
        }

        // 一次性清除并重建，避免清除与置位之间被并发观察到一个不存在的中间态。
        let _ = pollee.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
            Some((bits & !hangup_bits) | rebuilt)
        });

        if writable {
            pollee.fetch_or(
                (EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM).bits() as usize,
                Ordering::Relaxed,
            );
        } else {
            pollee.fetch_and(
                !(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM).bits() as usize,
                Ordering::Relaxed,
            );
        }

        if readable {
            pollee.fetch_or(
                (EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize,
                Ordering::Relaxed,
            );
        } else {
            pollee.fetch_and(
                !(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize,
                Ordering::Relaxed,
            );
        }
    }

    pub fn release(&self) {
        self.inner.release();
    }
}

#[derive(Debug)]
pub enum Inner {
    Init(Init),
    Connecting(Connecting),
    Listening(Listening),
    Established(Established),
    SelfConnected(SelfConnected),
    Closed(Closed),
}

impl Inner {
    pub fn with_socket<R, F>(&self, f: F) -> R
    where
        F: Fn(&smoltcp::socket::tcp::Socket<'static>) -> R,
    {
        match self {
            Inner::Init(init) => match init {
                Init::Unbound((socket, _)) => f(socket),
                Init::Bound(Bound { inner, .. }) => inner.with(f),
            },
            Inner::Connecting(conn) => conn.with(f),
            Inner::Listening(listen) => listen.inners[0].with(f),
            Inner::Established(est) => est.with(f),
            Inner::SelfConnected(_) => {
                // SelfConnected keeps a BoundInner for resource management, but does not
                // model its data path via smoltcp. Avoid touching the underlying socket.
                panic!("Inner::with_socket called on SelfConnected socket")
            }
            Inner::Closed(_) => {
                // Closed must not touch any smoltcp socket.
                // Callers should branch on Closed at a higher layer.
                panic!("Inner::with_socket called on Closed socket")
            }
        }
    }

    pub fn update_reuse_options(
        &mut self,
        reuseaddr: Option<bool>,
        reuseport: Option<bool>,
    ) -> Result<(), SystemError> {
        let binding = match self {
            Inner::Init(Init::Bound(bound)) => Some(&bound.binding),
            Inner::Connecting(conn) => Some(&conn.binding),
            Inner::Listening(listen) => Some(&listen.binding),
            Inner::Established(est) => est.binding.as_ref(),
            Inner::SelfConnected(sc) => Some(&sc.binding),
            _ => None,
        };
        if let Some(binding) = binding {
            binding.update_options(reuseaddr, reuseport)?;
        }
        // Listener identity remains stable across option changes. The namespace
        // chooses active members and refreshes transport eligibility before ingress.
        Ok(())
    }

    pub fn for_each_socket_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>),
    {
        match self {
            Inner::Init(init) => match init {
                Init::Unbound((socket, _)) => f(socket),
                Init::Bound(Bound { inner, .. }) => inner.with_mut(f),
            },
            Inner::Connecting(conn) => conn.with_mut(f),
            Inner::Listening(listen) => {
                for inner in &listen.inners {
                    inner.with_mut(&mut f);
                }
            }
            Inner::Established(est) => est.with_mut(f),
            Inner::SelfConnected(_) => {}
            Inner::Closed(_) => {}
        }
    }

    pub fn send_buffer_size(&self) -> usize {
        match self {
            Inner::Closed(_) => 0,
            Inner::SelfConnected(sc) => sc.send_capacity(),
            _ => self.with_socket(|socket| socket.send_capacity()),
        }
    }

    pub fn recv_buffer_size(&self) -> usize {
        match self {
            Inner::Closed(_) => 0,
            Inner::SelfConnected(sc) => sc.recv_capacity(),
            _ => self.with_socket(|socket| socket.recv_capacity()),
        }
    }

    pub fn iface(&self) -> Option<&alloc::sync::Arc<dyn crate::driver::net::Iface>> {
        match self {
            Inner::Init(Init::Bound(bound)) => Some(bound.inner.iface()),
            Inner::Init(Init::Unbound(_)) => None,
            Inner::Connecting(conn) => Some(conn.inner.iface()),
            Inner::Listening(listen) => Some(listen.inners[0].iface()),
            Inner::Established(est) => Some(est.inner.iface()),
            Inner::SelfConnected(sc) => Some(sc.inner.iface()),
            Inner::Closed(_) => None,
        }
    }

    pub fn local_endpoint(&self) -> smoltcp::wire::IpEndpoint {
        match self {
            Inner::Init(init) => match init {
                Init::Unbound((_, ver)) => match ver {
                    smoltcp::wire::IpVersion::Ipv4 => smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                        0,
                    ),
                    smoltcp::wire::IpVersion::Ipv6 => smoltcp::wire::IpEndpoint::new(
                        smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED),
                        0,
                    ),
                },
                Init::Bound(bound) => bound.binding.local,
            },
            Inner::Connecting(conn) => conn.get_name(),
            Inner::Listening(listen) => listen.get_name(),
            Inner::Established(est) => est.get_name(),
            Inner::SelfConnected(sc) => sc.get_name(),
            Inner::Closed(closed) => match closed.ver {
                smoltcp::wire::IpVersion::Ipv4 => smoltcp::wire::IpEndpoint::new(
                    smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
                    0,
                ),
                smoltcp::wire::IpVersion::Ipv6 => smoltcp::wire::IpEndpoint::new(
                    smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED),
                    0,
                ),
            },
        }
    }

    pub fn remote_endpoint(&self) -> Option<smoltcp::wire::IpEndpoint> {
        match self {
            Inner::Init(_) => None,
            Inner::Listening(_) => None,
            Inner::Connecting(conn) => Some(conn.get_peer_name()),
            Inner::Established(est) => Some(est.get_peer_name()),
            Inner::SelfConnected(sc) => Some(sc.get_peer_name()),
            Inner::Closed(_) => None,
        }
    }
}
