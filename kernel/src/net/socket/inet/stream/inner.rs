use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::filesystem::epoll::EPollEventType;
use crate::libs::mutex::Mutex;
use crate::libs::rwsem::RwSem;
use crate::net::socket::{self, inet::Types};
use crate::process::namespace::net_namespace::NetNamespace;
use alloc::boxed::Box;
use alloc::vec::Vec;
use smoltcp;
use smoltcp::socket::tcp;
use system_error::SystemError;

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
    smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer)
}

fn new_smoltcp_socket() -> smoltcp::socket::tcp::Socket<'static> {
    new_smoltcp_socket_with_size(DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE)
}

fn new_listen_smoltcp_socket<T>(
    local_endpoint: T,
) -> Result<smoltcp::socket::tcp::Socket<'static>, SystemError>
where
    T: Into<smoltcp::wire::IpListenEndpoint>,
{
    let mut socket = new_smoltcp_socket();
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
    Bound((socket::inet::BoundInner, smoltcp::wire::IpEndpoint)),
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

                *socket = Box::new(new_sock);
                Ok(())
            }
            Init::Bound((inner, _)) => {
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
    ) -> Result<Self, SystemError> {
        match self {
            Init::Unbound((socket, _)) => {
                let bound = socket::inet::BoundInner::bind(*socket, &local_endpoint.addr, netns)?;

                // Handle ephemeral port assignment (port 0)
                let bind_port = if local_endpoint.port == 0 {
                    bound.port_manager().bind_ephemeral_port(Types::Tcp)?
                } else {
                    bound
                        .port_manager()
                        .bind_port(Types::Tcp, local_endpoint.port)?;
                    local_endpoint.port
                };

                // Create endpoint with actual assigned port
                let final_endpoint = smoltcp::wire::IpEndpoint::new(local_endpoint.addr, bind_port);
                Ok(Init::Bound((bound, final_endpoint)))
            }
            Init::Bound(_) => {
                log::debug!("Already Bound");
                Err(SystemError::EINVAL)
            }
        }
    }

    pub(super) fn bind_to_ephemeral(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
    ) -> Result<(socket::inet::BoundInner, smoltcp::wire::IpEndpoint), (Self, SystemError)> {
        match self {
            Init::Unbound((socket, ver)) => {
                let (bound, address) =
                    socket::inet::BoundInner::bind_ephemeral(*socket, remote_endpoint.addr, netns)
                        .map_err(|err| (Self::new(ver), err))?;
                let bound_port = bound
                    .port_manager()
                    .bind_ephemeral_port(Types::Tcp)
                    .map_err(|err| (Self::new(ver), err))?;
                let endpoint = smoltcp::wire::IpEndpoint::new(address, bound_port);
                Ok((bound, endpoint))
            }
            Init::Bound(_) => Err((self, SystemError::EINVAL)),
        }
    }

    pub(super) fn connect(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
    ) -> Result<Connecting, (Self, SystemError)> {
        let (inner, local) = match self {
            Init::Unbound(_) => self.bind_to_ephemeral(remote_endpoint, netns)?,
            Init::Bound(inner) => inner,
        };
        if local.addr.is_unspecified() {
            return Err((Init::Bound((inner, local)), SystemError::EINVAL));
        }
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
            Ok(_) => Ok(Connecting::new(inner, local, remote_endpoint)),
            Err(err) => Err((Init::Bound((inner, local)), err)),
        }
    }

    /// # `listen`
    pub(super) fn listen(self, backlog: usize) -> Result<Listening, (Self, SystemError)> {
        let (inner, local) = match self {
            Init::Unbound(_) => {
                return Err((self, SystemError::EINVAL));
            }
            Init::Bound(inner) => inner,
        };
        let listen_addr = if local.addr.is_unspecified() {
            smoltcp::wire::IpListenEndpoint::from(local.port)
        } else {
            smoltcp::wire::IpListenEndpoint::from(local)
        };
        if listen_addr.port == 0 {
            // Invalid port number
            return Err((Init::Bound((inner, local)), SystemError::EINVAL));
        }
        // log::debug!("listen at {:?}, backlog {}", listen_addr, backlog);
        //
        // Linux semantics: listen(backlog=0) is valid. In practice it still allows
        // one pending connection in the accept queue (see sk_acceptq_is_full logic).
        // DragonOS uses multiple smoltcp TCP sockets to emulate accept queue slots.
        if backlog > u16::MAX as usize {
            return Err((Init::Bound((inner, local)), SystemError::EINVAL));
        }

        // Backlog emulation:
        // - backlog==0 => emulate a single accept slot
        // - cap to avoid excessive socket allocations (FIXME: refactor backlog mechanism)
        let backlog = core::cmp::min(if backlog == 0 { 1 } else { backlog }, 8);

        let mut inners = Vec::new();

        if let Err(err) = || -> Result<(), SystemError> {
            let additional_sockets = backlog.saturating_sub(1);
            for _ in 0..additional_sockets {
                // -1 because the first one is already bound
                // log::debug!("loop {:?}", _i);
                let new_listen = socket::inet::BoundInner::bind(
                    new_listen_smoltcp_socket(listen_addr)?,
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
            Ok(())
        }() {
            return Err((Init::Bound((inner, local)), err));
        }

        if let Err(err) = inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.listen(listen_addr).map_err(|err| match err {
                tcp::ListenError::InvalidState => SystemError::EINVAL,
                tcp::ListenError::Unaddressable => SystemError::EINVAL,
            })
        }) {
            return Err((Init::Bound((inner, local)), err));
        }

        inners.push(inner);
        return Ok(Listening {
            inners,
            connect: AtomicUsize::new(0),
            listen_addr,
        });
    }

    pub(super) fn close(&self) {
        match self {
            Init::Unbound(_) => {}
            Init::Bound((inner, endpoint)) => {
                inner.port_manager().unbind_port(Types::Tcp, endpoint.port);
                inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.close());
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
}

#[derive(Debug)]
pub struct Connecting {
    inner: socket::inet::BoundInner,
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
        local: smoltcp::wire::IpEndpoint,
        remote: smoltcp::wire::IpEndpoint,
    ) -> Self {
        Connecting {
            inner,
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

    pub fn into_result(self) -> (Inner, Result<(), SystemError>) {
        let result = *self.result.read();
        match result {
            ConnectResult::Connecting => (
                Inner::Connecting(self),
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            ),
            ConnectResult::Connected => (Inner::Established(Established::new(self.inner)), Ok(())),
            ConnectResult::Refused | ConnectResult::RefusedConsumed => {
                // unbind port
                self.inner
                    .port_manager()
                    .unbind_port(Types::Tcp, self.local.port);
                let socket = self.inner.into_socket();
                let socket = match socket {
                    smoltcp::socket::Socket::Tcp(s) => s,
                    _ => panic!("Connecting socket is not TCP"),
                };
                let ver = match self.local.addr {
                    smoltcp::wire::IpAddress::Ipv4(_) => smoltcp::wire::IpVersion::Ipv4,
                    smoltcp::wire::IpAddress::Ipv6(_) => smoltcp::wire::IpVersion::Ipv6,
                };
                (
                    Inner::Init(Init::Unbound((Box::new(socket), ver))),
                    Err(SystemError::ECONNREFUSED),
                )
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(*self.result.read(), ConnectResult::Connected)
    }

    /// Transmutes the Connecting state to Established state.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it forces a state transition without verifying
    /// that the underlying socket is actually in the ESTABLISHED state.
    /// The caller must ensure that the socket handshake has completed successfully.
    pub unsafe fn into_established(self) -> Established {
        Established::new(self.inner)
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
                    ConnectResult::Refused | ConnectResult::RefusedConsumed => {
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
                        if matches!(*result, ConnectResult::Refused) {
                            events_to_set |= EPollEventType::EPOLLERR;
                        }

                        pollee.fetch_or(events_to_set.bits() as usize, Ordering::Relaxed);

                        // If error IS consumed, clear EPOLLERR (if it was set previously)
                        if matches!(*result, ConnectResult::RefusedConsumed) {
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
        if matches!(*self.result.read(), ConnectResult::Refused) {
            Some(SystemError::ECONNREFUSED)
        } else {
            None
        }
    }

    pub fn consume_error(&self) {
        let mut guard = self.result.write();
        if matches!(*guard, ConnectResult::Refused) {
            *guard = ConnectResult::RefusedConsumed;
        }
    }

    pub fn is_refused_consumed(&self) -> bool {
        matches!(*self.result.read(), ConnectResult::RefusedConsumed)
    }
}

#[derive(Debug)]
pub struct Listening {
    pub inners: Vec<socket::inet::BoundInner>,
    connect: AtomicUsize,
    listen_addr: smoltcp::wire::IpListenEndpoint,
}

impl Listening {
    pub fn accept(&mut self) -> Result<(Established, smoltcp::wire::IpEndpoint), SystemError> {
        let connected: &mut socket::inet::BoundInner = self
            .inners
            .get_mut(self.connect.load(core::sync::atomic::Ordering::Relaxed))
            .unwrap();

        if connected.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| !socket.is_active()) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let remote_endpoint = connected.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket
                .remote_endpoint()
                .expect("A Connected Tcp With No Remote Endpoint")
        });

        // log::debug!("local at {:?}", local_endpoint);

        let mut new_listen = socket::inet::BoundInner::bind(
            new_listen_smoltcp_socket(self.listen_addr)?,
            self.listen_addr
                .addr
                .as_ref()
                .unwrap_or(&smoltcp::wire::IpAddress::from(
                    smoltcp::wire::Ipv4Address::UNSPECIFIED,
                )),
            connected.netns(),
        )?;

        // swap the connected socket with the new_listen socket
        // TODO is smoltcp socket swappable?
        core::mem::swap(&mut new_listen, connected);

        return Ok((Established::new(new_listen), remote_endpoint));
    }

    pub fn update_io_events(&self, pollee: &AtomicUsize) {
        // log::info!("Listening::update_io_events");
        let position = self.inners.iter().position(|inner| {
            inner.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.is_active())
        });

        if let Some(position) = position {
            self.connect
                .store(position, core::sync::atomic::Ordering::Relaxed);
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
        smoltcp::wire::IpEndpoint::new(
            self.listen_addr
                .addr
                .unwrap_or(smoltcp::wire::IpAddress::from(
                    smoltcp::wire::Ipv4Address::UNSPECIFIED,
                )),
            self.listen_addr.port,
        )
    }

    pub fn close(&self) {
        // log::debug!("Close Listening Socket");
        let port = self.get_name().port;
        for inner in self.inners.iter() {
            inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.close());
        }
        self.inners[0]
            .iface()
            .port_manager()
            .unbind_port(Types::Tcp, port);
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
}

impl Established {
    pub fn new(inner: socket::inet::BoundInner) -> Self {
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
        Self { inner, local, peer }
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

    pub fn close(&self) {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.close());
        self.inner.iface().poll();
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
                        smoltcp::socket::tcp::State::Closed
                        | smoltcp::socket::tcp::State::TimeWait
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
    inner: socket::inet::BoundInner,
    local: smoltcp::wire::IpEndpoint,
    /// Effective receive capacity for the loopback queue (bytes).
    rx_cap: AtomicUsize,
    buf: Mutex<VecDeque<u8>>,
}

impl SelfConnected {
    pub fn new(
        inner: socket::inet::BoundInner,
        local: smoltcp::wire::IpEndpoint,
        rx_cap: usize,
    ) -> Self {
        Self {
            inner,
            local,
            rx_cap: AtomicUsize::new(rx_cap),
            buf: Mutex::new(VecDeque::new()),
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
        self.buf.lock().len()
    }

    pub fn set_recv_buffer_size(&self, rx_size: usize) {
        self.rx_cap.store(rx_size, Ordering::Relaxed);
    }

    pub fn recv_capacity(&self) -> usize {
        self.rx_cap.load(Ordering::Relaxed)
    }

    pub fn send_capacity(&self) -> usize {
        // For self-connect, use the same capacity for "send" as the local receive queue.
        self.rx_cap.load(Ordering::Relaxed)
    }

    pub fn send_slice(&self, data: &[u8], send_shutdown: bool) -> Result<usize, SystemError> {
        if send_shutdown {
            return Err(SystemError::EPIPE);
        }
        if data.is_empty() {
            return Ok(0);
        }

        let cap = self.rx_cap.load(Ordering::Relaxed);
        let mut q = self.buf.lock();
        let free = cap.saturating_sub(q.len());
        if free == 0 {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let n = core::cmp::min(free, data.len());
        q.extend(&data[..n]);
        Ok(n)
    }

    pub fn recv_into(
        &self,
        out: &mut [u8],
        peek: bool,
        trunc: bool,
        send_shutdown: bool,
    ) -> Result<usize, SystemError> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut q = self.buf.lock();
        if q.is_empty() {
            // EOF after SHUT_WR once all queued data is drained.
            if send_shutdown {
                return Ok(0);
            }
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let n = core::cmp::min(out.len(), q.len());
        if !trunc {
            for (i, b) in q.iter().take(n).enumerate() {
                out[i] = *b;
            }
        }

        if !peek {
            for _ in 0..n {
                let _ = q.pop_front();
            }
        }
        Ok(n)
    }

    pub fn update_io_events(&self, pollee: &AtomicUsize, send_shutdown: bool) {
        let queued = self.recv_queue();
        let cap = self.rx_cap.load(Ordering::Relaxed);
        let writable = !send_shutdown && queued < cap;
        let readable = queued > 0 || send_shutdown; // readable after FIN to signal EOF

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
                Init::Bound((inner, _)) => inner.with(f),
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
                Init::Bound((inner, _)) => inner.with_mut(f),
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
            Inner::Init(_) => None,
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
                Init::Bound((_, local)) => *local,
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
