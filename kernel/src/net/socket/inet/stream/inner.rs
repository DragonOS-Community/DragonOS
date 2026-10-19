use crate::net::socket::inet::common::SocketDeviceBinding;
use alloc::sync::Arc;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::filesystem::epoll::EPollEventType;
use crate::libs::rwsem::RwSem;
use crate::mm::allocation::MemoryAllocationError;
use crate::net::socket::buffer::{try_owned_buffer, SocketMemory};
use crate::net::socket::{
    self,
    inet::common::port::{TcpBindDomain, TcpPortOwner, TcpPortReservation},
};
use crate::process::namespace::net_namespace::NetNamespace;
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

/// Actual listener handle-array storage. The protocol values themselves live
/// in SocketSet; this receipt covers only this retained allocation, not handles.
#[derive(Debug)]
pub(super) struct ListenerSlots {
    entries: Vec<super::bound::TcpBound>,
    _charge: crate::cgroup::memory::object::ObjectCharge,
}

impl ListenerSlots {
    pub(super) fn prepare(
        creator: &Arc<crate::cgroup::memory::object::ObjectAccount>,
        capacity: usize,
    ) -> Result<Self, MemoryAllocationError> {
        let layout = core::alloc::Layout::array::<super::bound::TcpBound>(capacity)
            .map_err(|_| SystemError::EOVERFLOW)?;
        let charge = creator.try_charge_allocation(
            crate::cgroup::memory::accounting::KernelByteKind::SlabUnreclaimable,
            layout,
        )?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self {
            entries,
            _charge: charge,
        })
    }
}

impl core::ops::Deref for ListenerSlots {
    type Target = Vec<super::bound::TcpBound>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}
impl core::ops::DerefMut for ListenerSlots {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}
impl<'a> IntoIterator for &'a ListenerSlots {
    type Item = &'a super::bound::TcpBound;
    type IntoIter = core::slice::Iter<'a, super::bound::TcpBound>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}
impl<'a> IntoIterator for &'a mut ListenerSlots {
    type Item = &'a mut super::bound::TcpBound;
    type IntoIter = core::slice::IterMut<'a, super::bound::TcpBound>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter_mut()
    }
}

pub(super) struct PreparedListenerStart {
    pub parent_id: u64,
    pub device: usize,
    pub observer: Arc<dyn tcp::LifecycleObserver>,
    pub slots: Option<ListenerSlots>,
}

pub(super) struct ListenerRefillSnapshot {
    pub parent_id: u64,
    pub owner: TcpPortOwner,
    pub port: u16,
    pub device: usize,
    pub domain: TcpBindDomain,
    pub listen_addr: smoltcp::wire::IpListenEndpoint,
    pub memory: SocketMemory,
    pub stack: Arc<crate::net::tcp_stack::TcpStack>,
    pub array_capacity: usize,
    pub pmtu_policies: [u8; 2],
    pub recv_error_policies: [bool; 2],
}

pub(super) struct PreparedListenerRefill {
    pub snapshot: ListenerRefillSnapshot,
    pub socket: Option<tcp::Socket<'static>>,
    pub slots: Option<ListenerSlots>,
}

impl PreparedListenerRefill {
    pub(super) fn prepare(
        snapshot: ListenerRefillSnapshot,
        rx: usize,
        tx: usize,
    ) -> Result<Self, MemoryAllocationError> {
        snapshot
            .stack
            .prepare_socket_capacity(&snapshot.memory.creator)?;
        let slots = if snapshot.array_capacity != 0 {
            Some(ListenerSlots::prepare(
                &snapshot.memory.creator,
                snapshot.array_capacity,
            )?)
        } else {
            None
        };
        let device = Arc::try_new(SocketDeviceBinding::from_ifindex(snapshot.device))
            .map_err(|_| SystemError::ENOMEM)?;
        let observer = snapshot.owner.prepare_child(snapshot.port, device)?;
        let mut socket = new_smoltcp_socket_with_size(rx, tx, &snapshot.memory)?;
        socket
            .set_listen_pmtu_policies(snapshot.pmtu_policies)
            .expect("validated listener policy");
        socket.set_listen_recv_error_policies(snapshot.recv_error_policies);
        socket.set_listen_ip_version(snapshot.domain.ip_version);
        socket.set_listen_bound_device(NonZeroU32::new(snapshot.device as u32));
        socket.set_lifecycle_observer(Some(observer));
        socket
            .listen(snapshot.listen_addr)
            .map_err(|_| SystemError::EINVAL)?;
        Ok(Self {
            snapshot,
            socket: Some(socket),
            slots,
        })
    }
}

/// Translate an accepted reset using the transport state before the RST.
pub(super) fn reset_error(state: tcp::State) -> SystemError {
    match state {
        tcp::State::SynSent => SystemError::ECONNREFUSED,
        tcp::State::CloseWait => SystemError::EPIPE,
        _ => SystemError::ECONNRESET,
    }
}

pub(super) fn transport_error(error: tcp::PendingError) -> SystemError {
    use tcp::{ErrorReason, NetworkError as N};
    match error.reason {
        ErrorReason::Reset => reset_error(error.state),
        ErrorReason::TimedOut => SystemError::ETIMEDOUT,
        ErrorReason::Network(reason) => match reason {
            N::NetworkUnreachable => SystemError::ENETUNREACH,
            N::HostUnreachable => SystemError::EHOSTUNREACH,
            N::ProtocolUnreachable => SystemError::ENOPROTOOPT,
            N::ConnectionRefused => SystemError::ECONNREFUSED,
            N::PermissionDenied => SystemError::EACCES,
            N::ProtocolError => SystemError::EPROTO,
            N::HostDown => SystemError::EHOSTDOWN,
            N::NoNetwork => SystemError::ENONET,
            N::OperationNotSupported => SystemError::EOPNOTSUPP_OR_ENOTSUP,
            N::MessageTooLarge => SystemError::EMSGSIZE,
        },
    }
}

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
    memory: &SocketMemory,
) -> Result<smoltcp::socket::tcp::Socket<'static>, MemoryAllocationError> {
    let rx_buffer = tcp::SocketBuffer::from_owned(try_owned_buffer(
        &memory.sock,
        &memory.creator,
        rx_size,
        0u8,
    )?);
    let tx_buffer = tcp::SocketBuffer::from_owned(try_owned_buffer(
        &memory.sock,
        &memory.creator,
        tx_size,
        0u8,
    )?);
    let mut socket = socket_with_buffers(rx_buffer, tx_buffer);
    assert!(socket.set_send_buffer_limit(tx_size));
    assert!(socket.set_receive_buffer_limit(rx_size));
    Ok(socket)
}

fn socket_with_buffers(
    rx_buffer: tcp::SocketBuffer<'static>,
    tx_buffer: tcp::SocketBuffer<'static>,
) -> tcp::Socket<'static> {
    let mut socket = smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer);
    socket.set_time_wait_duration(smoltcp::time::Duration::from_secs(60));
    socket.set_tsval_generator(Some(|| {
        let now: smoltcp::time::Instant = crate::time::Instant::now().into();
        now.total_millis() as u32
    }));
    socket
}

fn new_smoltcp_socket(
    memory: &SocketMemory,
) -> Result<smoltcp::socket::tcp::Socket<'static>, MemoryAllocationError> {
    new_smoltcp_socket_with_size(DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE, memory)
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Unbound state owns the actual protocol socket; transitions must not introduce locked allocation"
)]
pub enum Init {
    Unbound(
        (
            smoltcp::socket::tcp::Socket<'static>,
            smoltcp::wire::IpVersion,
        ),
    ),
    Bound(
        (
            super::bound::TcpBound,
            smoltcp::wire::IpEndpoint,
            TcpPortReservation,
        ),
    ),
}

impl Init {
    fn after_failed_connect(
        inner: super::bound::TcpBound,
        mut reservation: TcpPortReservation,
        ver: smoltcp::wire::IpVersion,
    ) -> Self {
        if let Some(domain) = reservation.locked_bind_domain {
            reservation.update_domain(domain);
            let local = smoltcp::wire::IpEndpoint::new(domain.addr, reservation.port);
            return Self::Bound((inner, local, reservation));
        }
        drop(reservation);
        let smoltcp::socket::Socket::Tcp(socket) = inner.into_socket() else {
            unreachable!("TCP binding contains TCP");
        };
        Self::Unbound((socket, ver))
    }

    pub(super) fn new(
        ver: smoltcp::wire::IpVersion,
        memory: &SocketMemory,
    ) -> Result<Self, MemoryAllocationError> {
        Ok(Init::Unbound((new_smoltcp_socket(memory)?, ver)))
    }

    #[expect(
        clippy::result_large_err,
        reason = "Failed bind restores the exact original protocol state without allocation"
    )]
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
                let bound = match super::bound::TcpBound::bind_recoverable(
                    socket,
                    &local_endpoint.addr,
                    netns.clone(),
                ) {
                    Ok(bound) => bound,
                    Err((socket, err)) => return Err((Init::Unbound((socket, ver)), err)),
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
                        return Err((Init::Unbound((socket, ver)), err));
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

    #[expect(
        clippy::result_large_err,
        reason = "Failed automatic bind returns the original protocol inventory"
    )]
    pub(super) fn bind_to_ephemeral(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        device_binding: Arc<SocketDeviceBinding>,
        owner: &TcpPortOwner,
    ) -> Result<
        (
            super::bound::TcpBound,
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
                    match super::bound::TcpBound::bind_ephemeral_recoverable_on_device(
                        socket,
                        remote_endpoint.addr,
                        netns.clone(),
                        device,
                    ) {
                        Ok(result) => result,
                        Err((socket, err)) => return Err((Self::Unbound((socket, ver)), err)),
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
                        return Err((Self::Unbound((socket, ver)), err));
                    }
                };
                let endpoint = smoltcp::wire::IpEndpoint::new(address, reservation.port);
                Ok((bound, endpoint, reservation))
            }
            Init::Bound(_) => Err((self, SystemError::EINVAL)),
        }
    }

    #[expect(
        clippy::result_large_err,
        reason = "Retry owns the original socket; failure cannot allocate a replacement owner"
    )]
    pub(super) fn connect(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        registration: &mut Option<ConnectingRegistration>,
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
                registration,
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
                    pending = Init::Unbound((socket, ver));
                }
                result => return result,
            }
        }
        Err((pending, SystemError::EADDRNOTAVAIL))
    }

    #[expect(
        clippy::result_large_err,
        reason = "Failed connection attempt returns original state without a failure-path allocation"
    )]
    fn connect_once(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        prepared_registration: &mut Option<ConnectingRegistration>,
        ver: smoltcp::wire::IpVersion,
        owner: &TcpPortOwner,
        automatic: bool,
    ) -> Result<Connecting, (Self, SystemError)> {
        let device_binding = owner.device_binding();
        let (inner, mut local, mut reservation) = match self {
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
        if local.addr.is_unspecified() {
            let target = device_binding.resolve_iface(&netns).and_then(|device| {
                socket::inet::common::tcp_connect_target(&remote_endpoint.addr, &netns, device)
            });
            let target = match target {
                Ok(target) => target,
                Err(err) => return Err((Init::Bound((inner, local, reservation)), err)),
            };
            local.addr = target.local_addr;
        }
        // Publish before taking the SocketSet lock and making the first SYN
        // visible to poll. The RAII reservation survives until Connecting is
        // replaced, while normal `bounds` registration covers the rest of the
        // socket lifetime.
        let result = {
            let mut sockets = inner.stack().sockets().lock();
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
            let mut interface = inner.stack().smol_iface().lock();
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
                    prepared_registration
                        .take()
                        .expect("task prepared connection registration"),
                    local,
                    remote_endpoint,
                    reservation,
                    ver,
                ))
            }
            Err(err) => Err((Init::Bound((inner, original_local, reservation)), err)),
        }
    }

    /// # `listen`
    ///
    /// Linux semantics: calling `listen()` on an unbound TCP socket auto-binds
    /// to `INADDR_ANY` (or `::`) with an ephemeral port, just like an implicit
    /// `bind(0.0.0.0:0)` before `listen()`.
    #[expect(
        clippy::result_large_err,
        reason = "Failed listen restores the exact unpublished protocol state"
    )]
    pub(super) fn listen(
        self,
        backlog: usize,
        device_binding: Arc<SocketDeviceBinding>,
        pmtu_policies: [u8; 2],
        prepared: &mut PreparedListenerStart,
        memory: SocketMemory,
    ) -> Result<Listening, (Self, SystemError)> {
        // Auto-bind and physical preparation are complete in the task outer.
        let (inner, local, reservation) = match self {
            Init::Bound(inner) => inner,
            unbound @ Init::Unbound(_) => return Err((unbound, SystemError::EINVAL)),
        };
        if reservation.id != prepared.parent_id || device_binding.ifindex() != prepared.device {
            return Err((
                Init::Bound((inner, local, reservation)),
                SystemError::EAGAIN_OR_EWOULDBLOCK,
            ));
        }
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
        // Keep logical capacity separate from allocated transport slots.
        let backlog = Listening::slot_capacity(backlog);

        let stack = inner.stack().clone();
        let recv_error_policies =
            inner.with::<tcp::Socket, _, _>(|socket| socket.recv_error_policies());
        let mut inner = inner;
        inner.pmtu_policies = pmtu_policies;
        let mut inners = prepared
            .slots
            .take()
            .expect("prepared initial listener array");
        let primary_index = inners.len();
        assert!(inners.len() < inners.capacity());
        inners.push(inner);

        if let Err(err) =
            inners[primary_index].with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                socket
                    .set_pmtu_discover(
                        pmtu_policies
                            [usize::from(local.addr.version() == smoltcp::wire::IpVersion::Ipv6)],
                    )
                    .expect("validated PMTU policy");
                socket.set_listen_ip_version(domain.ip_version);
                socket
                    .set_listen_pmtu_policies(pmtu_policies)
                    .expect("validated listener policies");
                socket.set_listen_recv_error_policies(recv_error_policies);
                socket.set_listen_bound_device(NonZeroU32::new(device_binding.ifindex() as u32));
                socket.set_lifecycle_observer(Some(prepared.observer.clone()));
                socket.listen(listen_addr).map_err(|err| match err {
                    tcp::ListenError::InvalidState => SystemError::EINVAL,
                    tcp::ListenError::Unaddressable => SystemError::EINVAL,
                })
            })
        {
            let inner = inners.remove(primary_index);
            prepared.slots = Some(inners);
            return Err((Init::Bound((inner, local, reservation)), err));
        }

        promotion.commit();
        return Ok(Listening {
            inners,
            stack,
            memory,
            refill_requested: false,
            refill_retry_at: 0,
            target_slots: backlog,
            shrink_pending: false,
            listen_addr,
            local,
            domain,
            reservation: Some(reservation),
            device_binding,
            pmtu_policies,
            recv_error_policies,
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

#[derive(Debug, Default, Clone)]
enum ConnectResult {
    Connected,
    #[default]
    Connecting,
    Failed(Option<SystemError>),
}

#[derive(Debug)]
pub struct Connecting {
    inner: super::bound::TcpBound,
    reservation: TcpPortReservation,
    ver: smoltcp::wire::IpVersion,
    registration: ConnectingRegistration,
    result: RwSem<ConnectResult>,
    local: smoltcp::wire::IpEndpoint,
    remote: smoltcp::wire::IpEndpoint,
}

impl Connecting {
    fn new(
        inner: super::bound::TcpBound,
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

    pub fn stack(&self) -> &Arc<crate::net::tcp_stack::TcpStack> {
        self.inner.stack()
    }

    pub(super) fn registration_publisher(&self) -> ConnectingRegistrationPublisher {
        self.registration.publisher()
    }

    pub fn into_result(mut self) -> (Inner, Result<(), SystemError>) {
        let result = self.with_mut(|socket| {
            let mut result = self.result.write();
            Self::refresh_result(socket, &mut result);
            result.clone()
        });
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
                        self.local,
                        self.remote,
                    )),
                    Ok(()),
                )
            }
            ConnectResult::Failed(error) => {
                let err = error.unwrap_or(SystemError::ECONNABORTED);
                (
                    Inner::Init(Init::after_failed_connect(
                        self.inner,
                        self.reservation,
                        self.ver,
                    )),
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
            self.local,
            self.remote,
        )
    }

    /// Converts a Connecting socket after its iface registration was removed
    /// by the close path. A delayed publisher observes cancellation and rolls
    /// back any registration racing with close.
    pub unsafe fn into_established_after_unbind(self) -> Established {
        self.registration.cancel();
        Established::new(self.inner, Some(self.reservation))
    }

    /// Resolve the handshake before an error consumer can remove the reset.
    /// Both the transport and result are locked by the caller, in that order.
    fn refresh_result(socket: &mut tcp::Socket, result: &mut ConnectResult) {
        if !matches!(result, ConnectResult::Connecting) {
            return;
        }
        let state = socket.state();
        let reset_state = socket.pending_error().map(|error| error.state);
        let completed = matches!(
            reset_state.unwrap_or(state),
            tcp::State::Established
                | tcp::State::CloseWait
                | tcp::State::FinWait1
                | tcp::State::FinWait2
                | tcp::State::Closing
                | tcp::State::LastAck
                | tcp::State::TimeWait
        );
        if completed {
            *result = ConnectResult::Connected;
        } else if let Some(error) = socket.take_error() {
            // Transfer a failed handshake's error into its result. It must not
            // remain independently consumable from the transport socket.
            *result = ConnectResult::Failed(Some(transport_error(error)));
        } else if !socket.is_open() {
            // Preserve the pre-existing non-RST connection failure behavior.
            *result = ConnectResult::Failed(Some(SystemError::ECONNREFUSED));
        }
    }

    #[must_use]
    pub(super) fn update_io_events(&self, pollee: &AtomicUsize) -> bool {
        self.inner.with_mut(|socket: &mut tcp::Socket| {
            let mut result = self.result.write();
            Self::refresh_result(socket, &mut result);
            let mut events = EPollEventType::empty();
            match &*result {
                ConnectResult::Connected => {
                    // The owner promotes this to Established before returning
                    // readiness; Established then publishes the transport state.
                    events = EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
                }
                ConnectResult::Failed(error) => {
                    events = EPollEventType::EPOLLIN
                        | EPollEventType::EPOLLRDNORM
                        | EPollEventType::EPOLLOUT
                        | EPollEventType::EPOLLWRNORM
                        | EPollEventType::EPOLLHUP
                        | EPollEventType::EPOLLRDHUP;
                    if error.is_some() {
                        events |= EPollEventType::EPOLLERR;
                    }
                }
                ConnectResult::Connecting => {}
            }
            pollee.store(events.bits() as usize, Ordering::Release);
            !matches!(*result, ConnectResult::Connecting)
        })
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        self.local
    }

    pub fn get_peer_name(&self) -> smoltcp::wire::IpEndpoint {
        self.remote
    }

    pub fn failure_reason(&self) -> Option<SystemError> {
        self.with_mut(|socket| {
            let mut result = self.result.write();
            Self::refresh_result(socket, &mut result);
            match &*result {
                ConnectResult::Failed(error) => error.clone(),
                _ => None,
            }
        })
    }

    pub fn take_error(&self) -> Option<SystemError> {
        self.take_error_inner(true)
    }

    /// Serialize a Linux error-queue dequeue with transport ingress and the
    /// handshake's transferred error. No second error owner is introduced.
    pub(super) fn consume_error_queue<R>(
        &self,
        mut consume: impl FnMut(Option<SystemError>) -> Option<(Option<SystemError>, R)>,
    ) -> Option<R> {
        self.with_mut(|socket| {
            let mut result = self.result.write();
            Self::refresh_result(socket, &mut result);
            let previous = match &*result {
                ConnectResult::Failed(error) => error.clone(),
                _ => socket.pending_error().map(transport_error),
            };
            let (next, value) = consume(previous)?;
            socket.take_error();
            match &mut *result {
                ConnectResult::Failed(error) => *error = next,
                _ => {
                    if let Some(error) = next {
                        socket.set_reported_network_error(super::icmp_error::network_reason(error));
                    }
                }
            }
            Some(value)
        })
    }

    /// A completed handshake must use the receive path, which drains data and
    /// honors a received FIN before considering a pending reset.
    pub fn take_connect_error(&self) -> Option<SystemError> {
        self.take_error_inner(false)
    }

    fn take_error_inner(&self, include_connected: bool) -> Option<SystemError> {
        self.with_mut(|socket| {
            let mut result = self.result.write();
            Self::refresh_result(socket, &mut result);
            match &mut *result {
                ConnectResult::Failed(error) => error.take(),
                ConnectResult::Connected if include_connected => {
                    socket.take_error().map(transport_error)
                }
                _ => None,
            }
        })
    }

    pub fn is_refused_consumed(&self) -> bool {
        matches!(*self.result.read(), ConnectResult::Failed(None))
    }

    pub fn set_shutdown_reset(&self) {
        *self.result.write() = ConnectResult::Failed(Some(SystemError::ECONNRESET));
    }
}

#[derive(Debug)]
pub struct Listening {
    pub(super) inners: ListenerSlots,
    pub(super) stack: Arc<crate::net::tcp_stack::TcpStack>,
    memory: SocketMemory,
    refill_requested: bool,
    refill_retry_at: u64,
    // Pending connections may temporarily keep the vector above this target.
    target_slots: usize,
    shrink_pending: bool,
    listen_addr: smoltcp::wire::IpListenEndpoint,
    local: smoltcp::wire::IpEndpoint,
    pub domain: TcpBindDomain,
    pub reservation: Option<TcpPortReservation>,
    device_binding: Arc<SocketDeviceBinding>,
    /// Future passive opens inherit the family-specific listener setting.
    /// Already-created children retain their own snapshot until accepted.
    pmtu_policies: [u8; 2],
    recv_error_policies: [bool; 2],
}

impl Listening {
    pub(super) fn set_recv_error_policy(
        &mut self,
        version: smoltcp::wire::IpVersion,
        enabled: bool,
    ) {
        self.recv_error_policies[usize::from(version == smoltcp::wire::IpVersion::Ipv6)] = enabled;
        let mut sockets = self.stack.sockets().lock();
        for bound in &self.inners {
            let socket = sockets.get_mut::<tcp::Socket>(bound.handle());
            // The next passive open snapshots these values under protocol
            // serialization; current children keep their inherited options.
            socket.set_listen_recv_error_policies(self.recv_error_policies);
        }
    }

    pub(super) fn defer_refill_retry(&mut self) {
        self.refill_requested = false;
        self.refill_retry_at =
            (crate::time::Instant::now().total_micros() as u64).saturating_add(1_000);
    }
    pub(super) fn take_refill_snapshot(&mut self) -> Option<ListenerRefillSnapshot> {
        if !self.refill_requested {
            return None;
        }
        self.refill_requested = false;
        if !self.needs_refill() {
            return None;
        }
        let reservation = self.reservation.as_ref()?;
        let array_capacity = if self.inners.len() == self.inners.capacity() {
            self.inners
                .capacity()
                .checked_mul(2)
                .filter(|n| *n > self.inners.len())?
        } else {
            0
        };
        Some(ListenerRefillSnapshot {
            parent_id: reservation.id,
            owner: reservation.owner(),
            port: self.local.port,
            device: self.device_binding.ifindex(),
            domain: self.domain,
            listen_addr: self.listen_addr,
            memory: self.memory.clone(),
            stack: self.stack.clone(),
            array_capacity,
            pmtu_policies: self.pmtu_policies,
            recv_error_policies: self.recv_error_policies,
        })
    }

    /// All physical storage is already born. Leave losers in `prepared` so
    /// their last storage/observer references retire after the caller's guard.
    pub(super) fn commit_refill(&mut self, prepared: &mut PreparedListenerRefill) -> bool {
        if !self.needs_refill()
            || self.reservation.as_ref().map(|r| r.id) != Some(prepared.snapshot.parent_id)
            || !Arc::ptr_eq(&self.stack, &prepared.snapshot.stack)
            || self.device_binding.ifindex() != prepared.snapshot.device
        {
            return false;
        }
        if self.inners.len() == self.inners.capacity()
            && prepared
                .slots
                .as_ref()
                .is_none_or(|slots| slots.capacity() <= self.inners.len())
        {
            return false;
        }
        prepared
            .socket
            .as_mut()
            .expect("prepared refill protocol")
            .set_listen_pmtu_policies(self.pmtu_policies)
            .expect("validated listener policies");
        prepared
            .socket
            .as_mut()
            .expect("prepared refill protocol")
            .set_listen_recv_error_policies(self.recv_error_policies);
        let socket = prepared.socket.take().expect("one pre-born refill socket");
        let mut bound = match super::bound::TcpBound::new(
            socket,
            self.stack
                .net_namespace()
                .expect("listener holds namespace"),
        ) {
            Ok(bound) => bound,
            Err((socket, _)) => {
                prepared.socket = Some(socket);
                return false;
            }
        };
        bound.pmtu_policies = self.pmtu_policies;
        if self.inners.len() == self.inners.capacity() {
            let mut slots = prepared
                .slots
                .take()
                .expect("prepared listener array covers live handles");
            slots.entries.append(&mut self.inners.entries);
            let retired = core::mem::replace(&mut self.inners, slots);
            prepared.slots = Some(retired);
        }
        assert!(self.inners.len() < self.inners.capacity());
        self.inners.push(bound);
        self.refill_retry_at = 0;
        true
    }

    pub(super) fn set_pmtu_discover(&mut self, version: smoltcp::wire::IpVersion, value: u8) {
        self.pmtu_policies[usize::from(version == smoltcp::wire::IpVersion::Ipv6)] = value;
        let Some(stack) = self.inners.first().map(|bound| bound.stack().clone()) else {
            return;
        };
        let mut sockets = stack.sockets().lock();
        for bound in &mut self.inners {
            let socket = sockets.get_mut::<tcp::Socket>(bound.handle());
            let idle = matches!(socket.state(), tcp::State::Listen | tcp::State::Closed);
            if idle {
                socket
                    .set_listen_pmtu_policies(self.pmtu_policies)
                    .expect("validated listener policies");
            }
            if idle {
                bound.pmtu_policies = self.pmtu_policies;
            }
        }
    }

    /// Update idle slots and the overflow lookup under the same SocketSet lock.
    /// Handshake/accepted snapshots remain owned by each TCP socket.
    pub(super) fn set_bound_device(&mut self, device: Option<NonZeroU32>) {
        {
            let mut sockets = self.stack.sockets().lock();
            for slot in &self.inners {
                sockets
                    .get_mut::<tcp::Socket>(slot.handle())
                    .set_listen_bound_device(device);
            }
            if let Some(reservation) = &self.reservation {
                self.stack.register_tcp_listener(
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
        // Linux admits one extra pending child (acceptq length > backlog).
        // sys_listen has already bounded backlog by somaxconn.
        backlog.saturating_add(1)
    }

    pub(super) fn prepare_syn(&mut self, local: smoltcp::wire::IpEndpoint, device: u32) -> bool {
        if local.port != self.local.port
            || !self.domain.matches(local.addr)
            || (self.device_binding.ifindex() != 0
                && self.device_binding.ifindex() != device as usize)
        {
            return false;
        }
        if self.shrink_pending {
            self.trim_excess();
        } else {
            self.rearm_closed_slots();
        }
        // Actual SYN matching selects its family policy inside smoltcp. This
        // hook rearms existing stock and requests preparation outside inner.
        if self.inners.len() >= self.target_slots
            || self.inners.iter().any(|bound| {
                bound.with::<tcp::Socket, _, _>(|socket| socket.state() == tcp::State::Listen)
            })
        {
            return false;
        }
        self.request_refill();
        self.refill_requested
    }

    pub(super) fn has_excess_slots(&self) -> bool {
        self.shrink_pending
    }

    fn can_remove_slot(&self, _index: usize) -> bool {
        self.inners.len() > self.target_slots
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

    fn rearm_closed_slots(&mut self) {
        // A retained child may have reset while other slots were removed.
        // Rearm it under the same lock as the state check. The nonzero local
        // endpoint was validated when the listener was created.
        let policies = self.pmtu_policies;
        for bound in &mut self.inners {
            let mut rearmed = false;
            bound.with_mut::<tcp::Socket, _, _>(|socket| {
                if socket.state() == tcp::State::Closed {
                    socket
                        .set_listen_pmtu_policies(policies)
                        .expect("validated PMTU policy");
                    socket.set_listen_bound_device(NonZeroU32::new(
                        self.device_binding.ifindex() as u32
                    ));
                    socket
                        .listen(self.listen_addr)
                        .expect("valid listener endpoint");
                    rearmed = true;
                }
            });
            if rearmed {
                bound.pmtu_policies = policies;
            }
        }
    }

    fn needs_refill(&self) -> bool {
        self.reservation.is_some()
            && self.inners.len() < self.target_slots
            && !self.inners.iter().any(|bound| {
                bound.with::<tcp::Socket, _, _>(|socket| socket.state() == tcp::State::Listen)
            })
    }

    fn request_refill(&mut self) {
        if self.needs_refill()
            && !self.refill_requested
            && crate::time::Instant::now().total_micros() as u64 >= self.refill_retry_at
        {
            self.refill_requested = true;
            self.stack.request_listener_refill();
        }
    }

    pub(super) fn set_backlog(&mut self, backlog: usize) -> Result<(), SystemError> {
        let target = Self::slot_capacity(backlog);
        self.target_slots = target;
        self.trim_excess();
        Ok(())
    }

    pub(super) fn peek_accept(&self) -> Option<(smoltcp::iface::SocketHandle, u64)> {
        self.inners.iter().find_map(|bound| {
            bound.with::<tcp::Socket, _, _>(|socket| {
                Self::accept_endpoints(socket)?;
                Some((bound.handle(), socket.lifecycle_observer()?.identity()))
            })
        })
    }

    pub(super) fn accept_prepared(
        &mut self,
        handle: smoltcp::iface::SocketHandle,
        binding: &crate::net::socket::inet::common::port::PreparedTcpAcceptBinding,
        target: &Arc<crate::cgroup::memory::socket::SockAccount>,
    ) -> Result<
        (
            Established,
            smoltcp::wire::IpEndpoint,
            Arc<SocketDeviceBinding>,
        ),
        MemoryAllocationError,
    > {
        let index = self
            .inners
            .iter()
            .position(|bound| bound.handle() == handle)
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        let bound = &self.inners[index];
        let (reservation, local, remote, retired_device) = {
            let mut sockets = self.stack.sockets().lock();
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            let (local, remote) =
                Self::accept_endpoints(socket).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
            if socket
                .lifecycle_observer()
                .map(|observer| observer.identity())
                != Some(binding.id)
            {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
            }
            let (rx, tx) = socket.owned_buffer_storage_mut();
            let rx = rx
                .and_then(|storage| {
                    storage
                        .as_any_mut()
                        .downcast_mut::<crate::net::socket::buffer::SockBuffer<u8>>()
                })
                .ok_or(SystemError::EINVAL)?;
            let tx = tx
                .and_then(|storage| {
                    storage
                        .as_any_mut()
                        .downcast_mut::<crate::net::socket::buffer::SockBuffer<u8>>()
                })
                .ok_or(SystemError::EINVAL)?;
            let (reservation, retired) =
                bound.netns().tcp_ports().claim_fd_prepared(binding, || {
                    crate::cgroup::memory::socket::SockAccount::transfer_birth_receipts(
                        &mut [rx.charge_mut(), tx.charge_mut()],
                        target,
                    )
                })?;
            (reservation, local, remote, retired)
        };
        let connected = self.inners.remove(index);
        self.request_refill();
        Ok((
            Established::with_endpoints(connected, Some(reservation), local, remote),
            remote,
            retired_device,
        ))
    }

    pub fn update_io_events(&mut self, pollee: &AtomicUsize) {
        // A retained pending child can reset after shrink has completed. It
        // must become a listen slot again, not permanently consume capacity.
        self.rearm_closed_slots();
        self.request_refill();
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
    inner: super::bound::TcpBound,
    local: smoltcp::wire::IpEndpoint,
    peer: smoltcp::wire::IpEndpoint,
    reservation: Option<TcpPortReservation>,
    connecting_registration: Option<ConnectingRegistrationLease>,
    /// Linux socket::state confirmation is independent of TCP transport state.
    /// Protected by TcpSocket::inner; I/O and readiness do not acknowledge it.
    connect_confirmed: bool,
}

impl Established {
    pub(super) fn pmtu_policies(&self) -> [u8; 2] {
        self.inner.pmtu_policies
    }
    pub fn port_owner(&self) -> TcpPortOwner {
        self.reservation
            .as_ref()
            .expect("established FD owns its TCP binding")
            .owner()
    }
    pub fn new(inner: super::bound::TcpBound, reservation: Option<TcpPortReservation>) -> Self {
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
        inner: super::bound::TcpBound,
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
            connect_confirmed: true,
        }
    }

    fn new_with_connecting_registration(
        inner: super::bound::TcpBound,
        reservation: Option<TcpPortReservation>,
        registration: ConnectingRegistrationLease,
        local: smoltcp::wire::IpEndpoint,
        peer: smoltcp::wire::IpEndpoint,
    ) -> Self {
        let mut established = Self::with_endpoints(inner, reservation, local, peer);
        established.connecting_registration = Some(registration);
        established.connect_confirmed = false;
        established
    }

    pub fn connect_confirmed(&self) -> bool {
        self.connect_confirmed
    }

    pub fn confirm_connect(&mut self) {
        self.connect_confirmed = true;
    }

    pub fn finish_connect(
        mut self,
        ver: smoltcp::wire::IpVersion,
    ) -> (Inner, Result<(), SystemError>) {
        if !self.connect_confirmed && self.with(|socket| socket.state() == tcp::State::Closed) {
            let error = self
                .with_mut(|socket| socket.take_error().map(transport_error))
                .unwrap_or(SystemError::ECONNABORTED);
            self.cancel_connecting_registration();
            let reservation = self
                .reservation
                .take()
                .expect("live connecting FD owns binding");
            return (
                Inner::Init(Init::after_failed_connect(self.inner, reservation, ver)),
                Err(error),
            );
        }
        self.confirm_connect();
        (Inner::Established(self), Ok(()))
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

    pub fn stack(&self) -> &Arc<crate::net::tcp_stack::TcpStack> {
        self.inner.stack()
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

    pub fn send_slice(&self, buf: &[u8], urgent_end: Option<usize>) -> Result<usize, SystemError> {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                // Background cork flushes also use this method. Return the
                // terminal send condition without consuming the reset; the
                // user-facing boundary atomically substitutes a pending error.
                if socket.can_send() {
                    socket
                        .send_slice_with_urgent(buf, urgent_end)
                        .map_err(|_| SystemError::ECONNABORTED)
                } else {
                    match socket.state() {
                        smoltcp::socket::tcp::State::Closed => Err(SystemError::EPIPE),
                        smoltcp::socket::tcp::State::TimeWait
                        | smoltcp::socket::tcp::State::Closing
                        | smoltcp::socket::tcp::State::LastAck => Err(SystemError::EPIPE),
                        _ => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                    }
                }
            })
    }

    pub fn update_io_events(
        &self,
        pollee: &AtomicUsize,
        shutdown: crate::net::socket::common::ShutdownBit,
    ) {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                let state = socket.state();
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

                use crate::net::socket::common::ShutdownBit;
                let read_closed = fin_received || shutdown.contains(ShutdownBit::SHUT_RD);
                let write_closed = is_closed || shutdown.contains(ShutdownBit::SHUT_WR);
                let mut events = EPollEventType::empty();
                if socket.can_send() || write_closed {
                    events |= EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
                }
                if socket.ordinary_can_recv() || read_closed {
                    events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
                }
                if read_closed {
                    events |= EPollEventType::EPOLLRDHUP;
                }
                // Linux tcp_fin records receive shutdown immediately. A local
                // SHUT_WR plus peer FIN is a full hangup even in CLOSING.
                if is_closed || (read_closed && write_closed) {
                    events |= EPollEventType::EPOLLHUP;
                }
                if matches!(socket.urgent_state(), Some(tcp::UrgentState::Valid(_))) {
                    events |= EPollEventType::EPOLLPRI;
                }
                if socket.pending_error().is_some() {
                    events |= EPollEventType::EPOLLERR;
                }
                // Publish one coherent snapshot while the transport lock is
                // held; concurrent refreshers cannot publish an older state.
                pollee.store(events.bits() as usize, Ordering::Release);
            })
    }
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Keep the protocol owner inline within the already allocated socket body"
)]
pub enum Inner {
    Init(Init),
    Connecting(Connecting),
    Listening(Listening),
    Established(Established),
    Closed(Closed),
}

impl Inner {
    /// A listener's accepted children have independent storage and limits.
    /// Changes to listener options apply to future slots, not existing children.
    pub(super) fn with_data_socket_mut<R>(
        &mut self,
        mut f: impl FnMut(&mut tcp::Socket<'static>) -> R,
    ) -> Option<R> {
        match self {
            Inner::Init(Init::Unbound((socket, _))) => Some(f(socket)),
            Inner::Init(Init::Bound((bound, _, _))) => Some(bound.with_mut(&mut f)),
            Inner::Connecting(connecting) => Some(connecting.with_mut(&mut f)),
            Inner::Established(established) => Some(established.with_mut(&mut f)),
            Inner::Listening(_) | Inner::Closed(_) => None,
        }
    }

    pub fn with_socket<R, F>(&self, f: F) -> Option<R>
    where
        F: Fn(&smoltcp::socket::tcp::Socket<'static>) -> R,
    {
        match self {
            Inner::Init(init) => match init {
                Init::Unbound((socket, _)) => Some(f(socket)),
                Init::Bound((inner, _, _)) => Some(inner.with(f)),
            },
            Inner::Connecting(conn) => Some(conn.with(f)),
            Inner::Listening(listen) => listen.inners.first().map(|bound| bound.with(f)),
            Inner::Established(est) => Some(est.with(f)),
            Inner::Closed(_) => None,
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

    pub fn stack(&self) -> Option<&alloc::sync::Arc<crate::net::tcp_stack::TcpStack>> {
        match self {
            Inner::Init(Init::Bound((inner, _, _))) => Some(inner.stack()),
            Inner::Init(Init::Unbound(_)) => None,
            Inner::Connecting(conn) => Some(conn.inner.stack()),
            Inner::Listening(listen) => Some(&listen.stack),
            Inner::Established(est) => Some(est.inner.stack()),
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

    /// The output path has a destination even while SYN_SENT has no public
    /// peer. Keep this identity query independent of getpeername semantics.
    pub(super) fn transport_remote_endpoint(&self) -> Option<smoltcp::wire::IpEndpoint> {
        match self {
            Inner::Init(_) => None,
            Inner::Listening(_) => None,
            Inner::Connecting(conn) => Some(conn.get_peer_name()),
            Inner::Established(est) => Some(est.get_peer_name()),
            Inner::Closed(_) => None,
        }
    }

    /// Observe the live transport without completing connect or consuming an
    /// error. The cached tuple alone does not establish a public peer.
    pub(super) fn peer_endpoint(&self) -> Option<smoltcp::wire::IpEndpoint> {
        let peer = |socket: &tcp::Socket, endpoint: smoltcp::wire::IpEndpoint| {
            // Linux detaches an inet_timewait_sock and closes the user socket;
            // smoltcp retains TIME_WAIT in the same handle instead.
            (endpoint.port != 0
                && !matches!(
                    socket.state(),
                    tcp::State::Closed
                        | tcp::State::Listen
                        | tcp::State::SynSent
                        | tcp::State::TimeWait
                ))
            .then_some(endpoint)
        };
        match self {
            Inner::Connecting(conn) => conn.with(|socket| peer(socket, conn.remote)),
            Inner::Established(est) => est.with(|socket| peer(socket, est.peer)),
            Inner::Init(_) | Inner::Listening(_) | Inner::Closed(_) => None,
        }
    }
}
