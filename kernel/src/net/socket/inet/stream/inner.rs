use crate::net::socket::inet::common::SocketDeviceBinding;
use alloc::sync::{Arc, Weak};
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::filesystem::epoll::EPollEventType;
use crate::libs::rwsem::RwSem;
use crate::net::socket::{
    self,
    inet::common::port::{TcpBindDomain, TcpPortOwner, TcpPortReservation},
};
use crate::process::namespace::net_namespace::NetNamespace;
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

/// 显式的“已关闭”状态：不再绑定/访问 smoltcp SocketSet 中的任何 handle。
///
/// 目的：
/// - 语义上表示 socket 已经 close；
/// - 并发上避免在 handle 已被 remove 后仍通过 update_events()/poll/notify 触达 SocketSet，
///   触发 smoltcp 的 "handle does not refer to a valid socket" panic。
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
    let mut socket = smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer);
    socket.set_time_wait_duration(smoltcp::time::Duration::from_secs(60));
    socket.set_tsval_generator(Some(|| {
        let now: smoltcp::time::Instant = crate::time::Instant::now().into();
        now.total_millis() as u32
    }));
    socket
}

fn new_smoltcp_socket() -> smoltcp::socket::tcp::Socket<'static> {
    new_smoltcp_socket_with_size(DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE)
}

fn new_listen_smoltcp_socket<T>(
    local_endpoint: T,
    ip_version: Option<smoltcp::wire::IpVersion>,
    device: Option<NonZeroU32>,
    reservation: &TcpPortReservation,
) -> Result<smoltcp::socket::tcp::Socket<'static>, SystemError>
where
    T: Into<smoltcp::wire::IpListenEndpoint>,
{
    let mut socket = new_smoltcp_socket();
    socket.set_listen_ip_version(ip_version);
    socket.set_listen_bound_device(device);
    socket.set_lifecycle_observer(Some(reservation.prepare_child(Arc::new(
        SocketDeviceBinding::from_ifindex(device.map_or(0, NonZeroU32::get) as usize),
    ))?));
    socket.listen(local_endpoint).map_err(|e| match e {
        tcp::ListenError::InvalidState => SystemError::EINVAL, // TODO: Check is right impl
        tcp::ListenError::Unaddressable => SystemError::EADDRINUSE,
    })?;
    Ok(socket)
}

#[derive(Debug)]
pub enum Init {
    Unbound(
        (
            Box<smoltcp::socket::tcp::Socket<'static>>,
            smoltcp::wire::IpVersion,
        ),
    ),
    Bound(
        (
            socket::inet::BoundInner,
            smoltcp::wire::IpEndpoint,
            TcpPortReservation,
        ),
    ),
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
                new_sock.set_listen_ip_version(socket.listen_ip_version());
                new_sock.set_bound_device(socket.bound_device());

                **socket = new_sock;
                Ok(())
            }
            Init::Bound((inner, _, _)) => {
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
        v6_only: bool,
        device_binding: Arc<SocketDeviceBinding>,
        owner: &TcpPortOwner,
    ) -> Result<Self, (Self, SystemError)> {
        match self {
            Init::Unbound((mut socket, ver)) => {
                socket.set_bound_device(NonZeroU32::new(device_binding.ifindex() as u32));
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

                let reservation = match owner.reserve(
                    TcpBindDomain::new(local_endpoint.addr, v6_only),
                    local_endpoint.port,
                    netns.local_port_range(),
                ) {
                    Ok(reservation) => reservation,
                    Err(err) => {
                        let smoltcp::socket::Socket::Tcp(socket) = bound.into_socket() else {
                            unreachable!("TCP BoundInner should contain a TCP socket");
                        };
                        return Err((Init::Unbound((Box::new(socket), ver)), err));
                    }
                };

                // Create endpoint with actual assigned port
                let final_endpoint =
                    smoltcp::wire::IpEndpoint::new(local_endpoint.addr, reservation.port);
                Ok(Init::Bound((bound, final_endpoint, reservation)))
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
        device_binding: Arc<SocketDeviceBinding>,
        owner: &TcpPortOwner,
    ) -> Result<
        (
            socket::inet::BoundInner,
            smoltcp::wire::IpEndpoint,
            TcpPortReservation,
        ),
        (Self, SystemError),
    > {
        match self {
            Init::Unbound((mut socket, ver)) => {
                let device = match device_binding.resolve_iface(&netns) {
                    Ok(device) => device,
                    Err(err) => return Err((Self::Unbound((socket, ver)), err)),
                };
                socket.set_bound_device(NonZeroU32::new(device_binding.ifindex() as u32));
                let (bound, address) =
                    match socket::inet::BoundInner::bind_ephemeral_recoverable_on_device(
                        *socket,
                        remote_endpoint.addr,
                        netns.clone(),
                        device,
                    ) {
                        Ok(result) => result,
                        Err((socket, err)) => {
                            return Err((Self::Unbound((Box::new(socket), ver)), err))
                        }
                    };
                let reservation = match owner.reserve_connect(
                    TcpBindDomain::new(address, false),
                    remote_endpoint,
                    netns.local_port_range(),
                ) {
                    Ok(reservation) => reservation,
                    Err(err) => {
                        let smoltcp::socket::Socket::Tcp(socket) = bound.into_socket() else {
                            unreachable!("TCP BoundInner should contain a TCP socket");
                        };
                        return Err((Self::Unbound((Box::new(socket), ver)), err));
                    }
                };
                let endpoint = smoltcp::wire::IpEndpoint::new(address, reservation.port);
                Ok((bound, endpoint, reservation))
            }
            Init::Bound(_) => Err((self, SystemError::EINVAL)),
        }
    }

    pub(super) fn connect(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        wrapper: Weak<dyn socket::inet::InetSocket>,
        ver: smoltcp::wire::IpVersion,
        owner: &TcpPortOwner,
    ) -> Result<Connecting, (Self, SystemError)> {
        let automatic = matches!(self, Init::Unbound(_));
        let (low, high) = netns.local_port_range();
        let attempts = if automatic {
            usize::from(high - low) + 1
        } else {
            1
        };
        let mut pending = self;
        for _ in 0..attempts {
            match pending.connect_once(
                remote_endpoint,
                netns.clone(),
                wrapper.clone(),
                ver,
                owner,
                automatic,
            ) {
                Err((Init::Bound((inner, _, reservation)), SystemError::EADDRNOTAVAIL))
                    if automatic =>
                {
                    // A candidate may collide with a protected TIME_WAIT after
                    // reservation. Retry the next ephemeral port without
                    // changing an explicit user binding or bypassing protection.
                    let smoltcp::socket::Socket::Tcp(mut socket) = inner.into_socket() else {
                        unreachable!("TCP connection candidate");
                    };
                    socket.set_lifecycle_observer(None);
                    drop(reservation);
                    pending = Init::Unbound((Box::new(socket), ver));
                }
                result => return result,
            }
        }
        Err((pending, SystemError::EADDRNOTAVAIL))
    }

    fn connect_once(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        wrapper: Weak<dyn socket::inet::InetSocket>,
        ver: smoltcp::wire::IpVersion,
        owner: &TcpPortOwner,
        automatic: bool,
    ) -> Result<Connecting, (Self, SystemError)> {
        let device_binding = owner.device_binding();
        let (mut inner, mut local, mut reservation) = match self {
            Init::Unbound(_) => self.bind_to_ephemeral(
                remote_endpoint,
                netns.clone(),
                device_binding.clone(),
                owner,
            )?,
            Init::Bound(inner) => inner,
        };
        if let Err(err) = socket::inet::common::ensure_bound_dual_stack_remote_compatible(
            local.addr,
            remote_endpoint.addr,
        ) {
            return Err((Init::Bound((inner, local, reservation)), err));
        }
        let original_local = local;
        let original_iface = inner.iface().clone();
        if local.addr.is_unspecified() {
            let target = device_binding.resolve_iface(&netns).and_then(|device| {
                socket::inet::common::tcp_connect_target(&remote_endpoint.addr, &netns, device)
            });
            let target = match target {
                Ok(target) => target,
                Err(err) => return Err((Init::Bound((inner, local, reservation)), err)),
            };
            if let Err(err) = inner.move_closed_tcp_to_iface(target.stack_owner) {
                return Err((Init::Bound((inner, local, reservation)), err));
            }
            local.addr = target.local_addr;
        }
        // Publish before taking the SocketSet lock and making the first SYN
        // visible to poll. The RAII reservation survives until Connecting is
        // replaced, while normal `bounds` registration covers the rest of the
        // socket lifetime.
        let registration = match ConnectingRegistration::try_new(inner.iface().clone(), wrapper) {
            Ok(registration) => registration,
            Err(err) => {
                inner
                    .move_closed_tcp_to_iface(original_iface)
                    .expect("inactive TCP placement rollback");
                return Err((Init::Bound((inner, original_local, reservation)), err));
            }
        };
        let result = {
            let mut sockets = inner.iface().sockets().lock();
            let socket = sockets.get_mut::<tcp::Socket>(inner.handle());
            socket.set_bound_device(NonZeroU32::new(device_binding.ifindex() as u32));
            socket.set_lifecycle_observer(Some(reservation.lifecycle_observer()));
            let is_loopback = |addr: smoltcp::wire::IpAddress| match addr {
                smoltcp::wire::IpAddress::Ipv4(addr) => addr.is_loopback(),
                smoltcp::wire::IpAddress::Ipv6(addr) => addr.is_loopback(),
            };
            let loopback = is_loopback(local.addr)
                || is_loopback(remote_endpoint.addr)
                || netns.loopback_iface().is_some_and(|iface| {
                    use crate::driver::net::Iface;
                    device_binding.ifindex() == iface.nic_id()
                });
            let policy = if automatic {
                tcp::TimeWaitReuse::Automatic { loopback }
            } else {
                tcp::TimeWaitReuse::Explicit
            };
            let mut interface = inner.iface().smol_iface().lock();
            let context = interface.context();
            // A rejected reuse attempt emits no packet and need not cause a
            // poll. Its safety deadline must use this transaction's time, not
            // the last received packet's time. Sample after both locks.
            context.now = crate::time::Instant::now().into();
            sockets
                .connect_tcp(inner.handle(), context, remote_endpoint, local, policy)
                .map_err(|err| match err {
                    tcp::ConnectError::AddressInUse => SystemError::EADDRNOTAVAIL,
                    tcp::ConnectError::InvalidState => SystemError::EINVAL,
                    tcp::ConnectError::Unaddressable => SystemError::ECONNREFUSED,
                })
        };
        match result {
            Ok(_) => {
                // Narrow only after the last fallible step. Widening on rollback
                // could overlap a bind admitted while the domain was narrower.
                if original_local.addr.is_unspecified() {
                    reservation.update_domain(TcpBindDomain::new(local.addr, false));
                }
                Ok(Connecting::new(
                    inner,
                    registration,
                    local,
                    remote_endpoint,
                    reservation,
                    ver,
                ))
            }
            Err(err) => {
                inner
                    .move_closed_tcp_to_iface(original_iface)
                    .expect("failed connect leaves TCP closed");
                Err((Init::Bound((inner, original_local, reservation)), err))
            }
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
        v6_only: bool,
        device_binding: Arc<SocketDeviceBinding>,
        owner: &TcpPortOwner,
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
            match self.bind(
                auto_bind_ep,
                netns.clone(),
                v6_only,
                device_binding.clone(),
                owner,
            ) {
                Ok(bound) => bound,
                Err((init, err)) => return Err((init, err)),
            }
        } else {
            self
        };
        let (inner, local, reservation) = match bound_self {
            Init::Bound(inner) => inner,
            Init::Unbound(_) => unreachable!(),
        };
        let domain = reservation.domain;
        let listen_addr = domain.listen_endpoint(local.port);
        if listen_addr.port == 0 {
            // Invalid port number
            return Err((
                Init::Bound((inner, local, reservation)),
                SystemError::EINVAL,
            ));
        }
        let promotion = match reservation.promote_listener() {
            Ok(promotion) => promotion,
            Err(err) => return Err((Init::Bound((inner, local, reservation)), err)),
        };
        // log::debug!("listen at {:?}, backlog {}", listen_addr, backlog);
        //
        // Linux semantics: listen(backlog=0) is valid. In practice it still allows
        // one pending connection in the accept queue (see sk_acceptq_is_full logic).
        // DragonOS uses multiple smoltcp TCP sockets to emulate accept queue slots.
        // Backlog emulation:
        // - backlog==0 => emulate a single accept slot
        // - cap to avoid excessive socket allocations (FIXME: refactor backlog mechanism)
        let backlog = Listening::slot_capacity(backlog);

        let mut inners = Vec::new();
        let is_any_addr = listen_addr.addr.is_none();

        if let Err(err) = || -> Result<(), SystemError> {
            if is_any_addr {
                // INADDR_ANY / [::]: smoltcp uses per-interface SocketSets, so we must
                // create at least one listen socket on *every* interface; otherwise a SYN
                // arriving on an interface without a listen socket gets no response (RST
                // or silent drop depending on smoltcp version).
                //
                // Establish interface coverage first; grow_slots below applies
                // the same capacity to every covered interface.
                let device_list = netns.device_list();
                for (_, iface) in device_list.iter() {
                    if alloc::sync::Arc::ptr_eq(iface, inner.iface()) {
                        continue; // primary inner already covers this iface
                    }
                    let new_listen = socket::inet::BoundInner::bind_on_iface(
                        new_listen_smoltcp_socket(
                            listen_addr,
                            domain.ip_version,
                            NonZeroU32::new(device_binding.ifindex() as u32),
                            &reservation,
                        )?,
                        iface.clone(),
                        inner.netns(),
                    )?;
                    inners.push(new_listen);
                }
            }
            Ok(())
        }() {
            for bound in &inners {
                bound.release();
            }
            return Err((Init::Bound((inner, local, reservation)), err));
        }

        let primary_index = inners.len();
        inners.push(inner);
        if let Err(err) = Listening::grow_slots(
            &mut inners,
            backlog,
            listen_addr,
            domain,
            &device_binding,
            &reservation,
        ) {
            let inner = inners.remove(primary_index);
            for bound in inners {
                bound.release();
            }
            return Err((Init::Bound((inner, local, reservation)), err));
        }

        if let Err(err) =
            inners[primary_index].with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                socket.set_listen_ip_version(domain.ip_version);
                socket.set_listen_bound_device(NonZeroU32::new(device_binding.ifindex() as u32));
                socket.set_lifecycle_observer(Some(reservation.prepare_child(Arc::new(
                    SocketDeviceBinding::from_ifindex(device_binding.ifindex()),
                ))?));
                socket.listen(listen_addr).map_err(|err| match err {
                    tcp::ListenError::InvalidState => SystemError::EINVAL,
                    tcp::ListenError::Unaddressable => SystemError::EINVAL,
                })
            })
        {
            let inner = inners.remove(primary_index);
            for bound in &inners {
                bound.release();
            }
            return Err((Init::Bound((inner, local, reservation)), err));
        }

        promotion.commit();
        return Ok(Listening {
            inners,
            slots_per_iface: backlog,
            shrink_pending: false,
            listen_addr,
            local,
            domain,
            reservation: Some(reservation),
            device_binding,
        });
    }

    pub(super) fn close(self) -> Closed {
        match self {
            Init::Unbound((_, version)) => Closed::new(version),
            Init::Bound((inner, endpoint, reservation)) => {
                drop(reservation);
                let version = match endpoint.addr {
                    smoltcp::wire::IpAddress::Ipv4(_) => smoltcp::wire::IpVersion::Ipv4,
                    smoltcp::wire::IpAddress::Ipv6(_) => smoltcp::wire::IpVersion::Ipv6,
                };
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
    inner: socket::inet::BoundInner,
    reservation: TcpPortReservation,
    ver: smoltcp::wire::IpVersion,
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
        local: smoltcp::wire::IpEndpoint,
        remote: smoltcp::wire::IpEndpoint,
        reservation: TcpPortReservation,
        ver: smoltcp::wire::IpVersion,
    ) -> Self {
        Connecting {
            inner,
            reservation,
            ver,
            registration,
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
                        Some(self.reservation),
                        registration,
                    )),
                    Ok(()),
                )
            }
            ConnectResult::Refused
            | ConnectResult::RefusedConsumed
            | ConnectResult::ShutdownReset
            | ConnectResult::ShutdownResetConsumed => {
                let err = match result {
                    ConnectResult::ShutdownReset | ConnectResult::ShutdownResetConsumed => {
                        SystemError::ECONNRESET
                    }
                    _ => SystemError::ECONNREFUSED,
                };
                if let Some(domain) = self.reservation.locked_bind_domain {
                    // Like tcp_disconnect(), restore the user's bind address
                    // after source selection, but retain a nonzero port lock.
                    self.reservation.update_domain(domain);
                    let local = smoltcp::wire::IpEndpoint::new(domain.addr, self.reservation.port);
                    return (
                        Inner::Init(Init::Bound((self.inner, local, self.reservation))),
                        Err(err),
                    );
                }
                drop(self.reservation);
                let socket = self.inner.into_socket();
                let socket = match socket {
                    smoltcp::socket::Socket::Tcp(s) => s,
                    _ => panic!("Connecting socket is not TCP"),
                };
                (
                    Inner::Init(Init::Unbound((Box::new(socket), self.ver))),
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
        Established::new_with_connecting_registration(
            self.inner,
            Some(self.reservation),
            registration,
        )
    }

    /// Converts a Connecting socket after its iface registration was removed
    /// by the close path. A delayed publisher observes cancellation and rolls
    /// back any registration racing with close.
    pub unsafe fn into_established_after_unbind(self) -> Established {
        self.registration.cancel();
        Established::new(self.inner, Some(self.reservation))
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
    // Pending connections may temporarily keep the vector above this target.
    slots_per_iface: usize,
    shrink_pending: bool,
    listen_addr: smoltcp::wire::IpListenEndpoint,
    local: smoltcp::wire::IpEndpoint,
    pub domain: TcpBindDomain,
    pub reservation: Option<TcpPortReservation>,
    device_binding: Arc<SocketDeviceBinding>,
}

impl Listening {
    /// Update idle slots and the overflow lookup under the same SocketSet lock.
    /// Handshake/accepted snapshots remain owned by each TCP socket.
    pub(super) fn set_bound_device(&mut self, device: Option<NonZeroU32>) {
        for (index, bound) in self.inners.iter().enumerate() {
            if self.inners[..index]
                .iter()
                .any(|other| Arc::ptr_eq(other.iface(), bound.iface()))
            {
                continue;
            }
            let mut sockets = bound.iface().sockets().lock();
            for slot in &self.inners {
                if Arc::ptr_eq(slot.iface(), bound.iface()) {
                    sockets
                        .get_mut::<tcp::Socket>(slot.handle())
                        .set_listen_bound_device(device);
                }
            }
            if let Some(reservation) = &self.reservation {
                bound.iface().common().register_tcp_listener(
                    reservation.id,
                    self.domain,
                    self.local.port,
                    device.map_or(0, NonZeroU32::get),
                );
            }
        }
    }

    /// Ordinary passive opens become acceptable only after the final ACK.
    /// A peer may already have sent FIN, so CLOSE_WAIT remains acceptable.
    /// Snapshot both endpoints under the caller's SocketSet lock: a later RST
    /// can clear the tuple before the accepted socket is constructed.
    fn accept_endpoints(
        socket: &tcp::Socket,
    ) -> Option<(smoltcp::wire::IpEndpoint, smoltcp::wire::IpEndpoint)> {
        if matches!(
            socket.state(),
            tcp::State::Established | tcp::State::CloseWait
        ) {
            Some((socket.local_endpoint()?, socket.remote_endpoint()?))
        } else {
            None
        }
    }

    fn slot_capacity(backlog: usize) -> usize {
        // Bounded per-interface emulation, not Linux's global accept queue.
        // At 256 KiB per socket this uses at most 2 MiB per interface.
        backlog.clamp(1, 8)
    }

    pub(super) fn has_excess_slots(&self) -> bool {
        self.shrink_pending
    }

    fn can_remove_slot(&self, index: usize) -> bool {
        self.inners
            .iter()
            .filter(|bound| Arc::ptr_eq(bound.iface(), self.inners[index].iface()))
            .count()
            > self.slots_per_iface
    }

    pub(super) fn trim_excess(&mut self) {
        // Reap dead children before idle slots, so shrinking does not retain a
        // dead handle instead of a healthy listener on that interface.
        for state in [tcp::State::Closed, tcp::State::Listen] {
            let mut index = 0;
            while index < self.inners.len() {
                let remove = self.can_remove_slot(index)
                    && self.inners[index].with_mut::<tcp::Socket, _, _>(|socket| {
                        if socket.state() == state {
                            // Check and close under the same SocketSet lock so a
                            // concurrent poll cannot consume the slot in between.
                            socket.close();
                            true
                        } else {
                            false
                        }
                    });
                if remove {
                    self.inners.remove(index).release();
                } else {
                    index += 1;
                }
            }
        }
        self.shrink_pending = (0..self.inners.len()).any(|index| self.can_remove_slot(index));
        self.rearm_closed_slots();
    }

    fn rearm_closed_slots(&self) {
        // A retained child may have reset while other slots were removed.
        // Rearm it under the same lock as the state check. The nonzero local
        // endpoint was validated when the listener was created.
        for bound in &self.inners {
            bound.with_mut::<tcp::Socket, _, _>(|socket| {
                if socket.state() == tcp::State::Closed {
                    socket.set_listen_bound_device(NonZeroU32::new(
                        self.device_binding.ifindex() as u32
                    ));
                    socket
                        .listen(self.listen_addr)
                        .expect("valid listener endpoint");
                }
            });
        }
    }

    fn grow_slots(
        inners: &mut Vec<socket::inet::BoundInner>,
        target: usize,
        listen_addr: smoltcp::wire::IpListenEndpoint,
        domain: TcpBindDomain,
        device_binding: &SocketDeviceBinding,
        reservation: &TcpPortReservation,
    ) -> Result<(), SystemError> {
        // Prepare sockets before publishing any new handles; keep existing
        // connections and the previous capacity on a recoverable failure.
        let mut sockets = Vec::new();
        for (index, bound) in inners.iter().enumerate() {
            if inners[..index]
                .iter()
                .any(|other| Arc::ptr_eq(other.iface(), bound.iface()))
            {
                continue;
            }
            let count = inners
                .iter()
                .filter(|other| Arc::ptr_eq(other.iface(), bound.iface()))
                .count();
            for _ in count..target {
                sockets.push((
                    new_listen_smoltcp_socket(
                        listen_addr,
                        domain.ip_version,
                        NonZeroU32::new(device_binding.ifindex() as u32),
                        reservation,
                    )?,
                    bound.iface().clone(),
                    bound.netns(),
                ));
            }
        }
        let mut added = Vec::new();
        for (socket, iface, netns) in sockets {
            match socket::inet::BoundInner::bind_on_iface(socket, iface, netns) {
                Ok(bound) => added.push(bound),
                Err(err) => {
                    for bound in added {
                        bound.release();
                    }
                    return Err(err);
                }
            }
        }
        inners.extend(added);
        Ok(())
    }

    pub(super) fn set_backlog(&mut self, backlog: usize) -> Result<(), SystemError> {
        let target = Self::slot_capacity(backlog);
        Self::grow_slots(
            &mut self.inners,
            target,
            self.listen_addr,
            self.domain,
            &self.device_binding,
            self.reservation
                .as_ref()
                .expect("open listener reservation"),
        )?;
        self.slots_per_iface = target;
        self.trim_excess();
        Ok(())
    }

    pub fn accept(&mut self) -> Result<(Established, smoltcp::wire::IpEndpoint), SystemError> {
        // Resizing can invalidate vector indices. Select the current live slot
        // under the caller's inner write lock instead of caching a poll index.
        let index = self
            .inners
            .iter()
            .enumerate()
            .find_map(|(index, bound)| {
                bound.with::<tcp::Socket, _, _>(|socket| {
                    Self::accept_endpoints(socket).map(|_| index)
                })
            })
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

        let retire = self.can_remove_slot(index);
        let connected = &mut self.inners[index];

        if retire {
            let (reservation, local_endpoint, remote_endpoint) = Self::claim_child(connected)?;
            let connected = self.inners.remove(index);
            return Ok((
                Established::with_endpoints(
                    connected,
                    Some(reservation),
                    local_endpoint,
                    remote_endpoint,
                ),
                remote_endpoint,
            ));
        }

        // log::debug!("local at {:?}", local_endpoint);

        // Create a replacement listen socket on the *same* interface as the one
        // that just accepted a connection. This is critical for INADDR_ANY listeners
        // where each interface has its own listen socket in the smoltcp SocketSet.
        let mut new_listen = if self.listen_addr.addr.is_none() {
            socket::inet::BoundInner::bind_on_iface(
                new_listen_smoltcp_socket(
                    self.listen_addr,
                    self.domain.ip_version,
                    NonZeroU32::new(self.device_binding.ifindex() as u32),
                    self.reservation
                        .as_ref()
                        .expect("open listener reservation"),
                )?,
                connected.iface().clone(),
                connected.netns(),
            )?
        } else {
            socket::inet::BoundInner::bind(
                new_listen_smoltcp_socket(
                    self.listen_addr,
                    self.domain.ip_version,
                    NonZeroU32::new(self.device_binding.ifindex() as u32),
                    self.reservation
                        .as_ref()
                        .expect("open listener reservation"),
                )?,
                self.listen_addr
                    .addr
                    .as_ref()
                    .unwrap_or(&smoltcp::wire::IpAddress::from(
                        smoltcp::wire::Ipv4Address::UNSPECIFIED,
                    )),
                connected.netns(),
            )?
        };

        let (reservation, local_endpoint, remote_endpoint) = match Self::claim_child(connected) {
            Ok(child) => child,
            Err(err) => {
                new_listen.release();
                return Err(err);
            }
        };

        // swap the connected socket with the new_listen socket
        // TODO is smoltcp socket swappable?
        core::mem::swap(&mut new_listen, connected);

        return Ok((
            Established::with_endpoints(
                new_listen,
                Some(reservation),
                local_endpoint,
                remote_endpoint,
            ),
            remote_endpoint,
        ));
    }

    fn claim_child(
        bound: &socket::inet::BoundInner,
    ) -> Result<
        (
            TcpPortReservation,
            smoltcp::wire::IpEndpoint,
            smoltcp::wire::IpEndpoint,
        ),
        SystemError,
    > {
        bound.with::<tcp::Socket, _, _>(|socket| {
            let (local, remote) =
                Self::accept_endpoints(socket).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
            let observer = socket.lifecycle_observer().ok_or(SystemError::EINVAL)?;
            let reservation = bound.netns().tcp_ports().claim_fd(observer.identity())?;
            Ok((reservation, local, remote))
        })
    }

    pub fn update_io_events(&self, pollee: &AtomicUsize) {
        // A retained pending child can reset after shrink has completed. It
        // must become a listen slot again, not permanently consume capacity.
        self.rearm_closed_slots();
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

        // log::info!("Listening::update_io_events");
        let ready = self.inners.iter().any(|inner| {
            inner.with::<tcp::Socket, _, _>(|socket| Self::accept_endpoints(socket).is_some())
        });

        if ready {
            pollee.fetch_or(
                EPollEventType::EPOLL_LISTEN_CAN_ACCEPT.bits() as usize,
                core::sync::atomic::Ordering::Relaxed,
            );
        } else {
            pollee.fetch_and(
                !EPollEventType::EPOLL_LISTEN_CAN_ACCEPT.bits() as usize,
                core::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        self.local
    }

    pub fn close(&mut self) {
        // log::debug!("Close Listening Socket");
        for inner in self.inners.iter() {
            inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.close());
        }
        self.reservation.take();
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
    reservation: Option<TcpPortReservation>,
    connecting_registration: Option<ConnectingRegistrationLease>,
}

impl Established {
    pub fn port_owner(&self) -> TcpPortOwner {
        self.reservation
            .as_ref()
            .expect("established FD owns its TCP binding")
            .owner()
    }
    pub fn new(inner: socket::inet::BoundInner, reservation: Option<TcpPortReservation>) -> Self {
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
        Self::with_endpoints(inner, reservation, local, peer)
    }

    fn with_endpoints(
        inner: socket::inet::BoundInner,
        reservation: Option<TcpPortReservation>,
        local: smoltcp::wire::IpEndpoint,
        peer: smoltcp::wire::IpEndpoint,
    ) -> Self {
        Self {
            inner,
            local,
            peer,
            reservation,
            connecting_registration: None,
        }
    }

    fn new_with_connecting_registration(
        inner: socket::inet::BoundInner,
        reservation: Option<TcpPortReservation>,
        registration: ConnectingRegistrationLease,
    ) -> Self {
        let mut established = Self::new(inner, reservation);
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

    pub fn release_port(&mut self) {
        self.reservation.take();
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

#[derive(Debug)]
pub enum Inner {
    Init(Init),
    Connecting(Connecting),
    Listening(Listening),
    Established(Established),
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
                Init::Bound((inner, _, _)) => inner.with(f),
            },
            Inner::Connecting(conn) => conn.with(f),
            Inner::Listening(listen) => listen.inners[0].with(f),
            Inner::Established(est) => est.with(f),
            Inner::Closed(_) => {
                // Closed 状态不应再触达任何 smoltcp socket。
                // 调用者应当在更上层对 Closed 做分支处理。
                panic!("Inner::with_socket called on Closed socket")
            }
        }
    }

    pub fn for_each_socket_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>),
    {
        match self {
            Inner::Init(init) => match init {
                Init::Unbound((socket, _)) => f(socket),
                Init::Bound((inner, _, _)) => inner.with_mut(f),
            },
            Inner::Connecting(conn) => conn.with_mut(f),
            Inner::Listening(listen) => {
                for inner in &listen.inners {
                    inner.with_mut(&mut f);
                }
            }
            Inner::Established(est) => est.with_mut(f),
            Inner::Closed(_) => {}
        }
    }

    pub fn send_buffer_size(&self) -> usize {
        match self {
            Inner::Closed(_) => 0,
            _ => self.with_socket(|socket| socket.send_capacity()),
        }
    }

    pub fn recv_buffer_size(&self) -> usize {
        match self {
            Inner::Closed(_) => 0,
            _ => self.with_socket(|socket| socket.recv_capacity()),
        }
    }

    pub fn iface(&self) -> Option<&alloc::sync::Arc<dyn crate::driver::net::Iface>> {
        match self {
            Inner::Init(Init::Bound((inner, _, _))) => Some(inner.iface()),
            Inner::Init(Init::Unbound(_)) => None,
            Inner::Connecting(conn) => Some(conn.inner.iface()),
            Inner::Listening(listen) => Some(listen.inners[0].iface()),
            Inner::Established(est) => Some(est.inner.iface()),
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
                Init::Bound((_, local, _)) => *local,
            },
            Inner::Connecting(conn) => conn.get_name(),
            Inner::Listening(listen) => listen.get_name(),
            Inner::Established(est) => est.get_name(),
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
            Inner::Closed(_) => None,
        }
    }
}
