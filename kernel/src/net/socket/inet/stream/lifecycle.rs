use crate::net::socket::common::ShutdownBit;
use crate::net::socket::inet::InetSocket;
use crate::net::tcp_close_defer::{
    DeferredTcpCloseKind, DeferredTcpCloseReason, DeferredTcpCloseRequest,
};
use alloc::sync::Arc;
use system_error::SystemError;

use super::inner;
use super::TcpSocket;

const TCP_ESTABLISHED_POST_POLL_ROUNDS: usize = 8;
const TCP_ESTABLISHED_CLOSE_POST_POLL_ROUNDS: usize = 1;
const TCP_CONNECTING_ABORT_POST_POLL_ROUNDS: usize = 128;
const TCP_LISTEN_POST_POLL_MIN_ROUNDS: usize = 128;
const TCP_LISTEN_POST_POLL_MAX_ROUNDS: usize = 8192;
const TCP_LISTEN_POST_POLL_ROUNDS_PER_SOCKET: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CloseObservation {
    state: smoltcp::socket::tcp::State,
    unread: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CloseAction {
    kind: DeferredTcpCloseKind,
    reason: DeferredTcpCloseReason,
    abort_on_post_close_data: bool,
}

impl TcpSocket {
    /// Normalize mapped sockaddr input while holding `inner`, so a concurrent
    /// IPV6_V6ONLY update cannot change the domain between validation and bind.
    fn normalize_endpoint(
        &self,
        mut endpoint: smoltcp::wire::IpEndpoint,
        connecting: bool,
    ) -> Result<smoltcp::wire::IpEndpoint, SystemError> {
        use smoltcp::wire::{IpAddress, IpVersion};
        if endpoint.addr.version() != self.ip_version {
            return Err(SystemError::EAFNOSUPPORT);
        }
        if let IpAddress::Ipv6(addr) = endpoint.addr {
            if let Some(mapped) = addr.to_ipv4_mapped() {
                if self
                    .options
                    .ipv6_only
                    .load(core::sync::atomic::Ordering::Relaxed)
                {
                    return Err(if connecting {
                        SystemError::ENETUNREACH
                    } else {
                        SystemError::EINVAL
                    });
                }
                endpoint.addr = IpAddress::Ipv4(mapped);
            } else if !connecting && addr.is_multicast() && self.ip_version == IpVersion::Ipv6 {
                return Err(SystemError::EINVAL);
            }
        }
        Ok(endpoint)
    }

    fn kick_iface_after_tcp_state_change(
        iface: &Arc<dyn crate::net::Iface>,
        poll_rounds: usize,
        notify_bound_sockets: bool,
    ) {
        if let Some(netns) = iface.common().net_namespace() {
            netns.wakeup_poll_thread();
        }

        for _ in 0..poll_rounds {
            let _ = iface.poll();
        }

        if notify_bound_sockets {
            // Listener shutdown/close changes a shared endpoint. Refresh all bound sockets so
            // blocked accept/connect users observe the final state without requiring a new packet.
            iface.common().notify_all_bound_sockets();
        }
    }

    #[inline]
    fn listen_post_poll_rounds(socket_count: usize) -> usize {
        socket_count
            .saturating_mul(TCP_LISTEN_POST_POLL_ROUNDS_PER_SOCKET)
            .clamp(
                TCP_LISTEN_POST_POLL_MIN_ROUNDS,
                TCP_LISTEN_POST_POLL_MAX_ROUNDS,
            )
    }

    #[inline]
    fn observe_established_close(established: &inner::Established) -> CloseObservation {
        established.with(|socket| CloseObservation {
            state: socket.state(),
            unread: socket.recv_queue(),
        })
    }

    fn decide_established_close(&self, established: &inner::Established) -> CloseAction {
        let observation = Self::observe_established_close(established);

        if observation.unread > 0 {
            return CloseAction {
                kind: DeferredTcpCloseKind::Reset,
                reason: DeferredTcpCloseReason::UnreadDataOnClose,
                abort_on_post_close_data: false,
            };
        }

        if self.should_abort_established_zero_linger(observation.state) {
            return CloseAction {
                kind: DeferredTcpCloseKind::Reset,
                reason: DeferredTcpCloseReason::ZeroLinger,
                abort_on_post_close_data: false,
            };
        }

        let abort_on_post_close_data = matches!(
            observation.state,
            smoltcp::socket::tcp::State::Established | smoltcp::socket::tcp::State::SynReceived
        );

        CloseAction {
            kind: DeferredTcpCloseKind::GracefulFin,
            reason: DeferredTcpCloseReason::NormalClose,
            abort_on_post_close_data,
        }
    }

    #[inline]
    fn is_zero_linger_close(&self) -> bool {
        self.linger_onoff()
            .load(core::sync::atomic::Ordering::Relaxed)
            != 0
            && self
                .linger_linger()
                .load(core::sync::atomic::Ordering::Relaxed)
                == 0
    }

    #[inline]
    fn should_abort_established_zero_linger(&self, state: smoltcp::socket::tcp::State) -> bool {
        if !self.is_zero_linger_close() {
            return false;
        }

        matches!(
            state,
            smoltcp::socket::tcp::State::SynReceived
                | smoltcp::socket::tcp::State::Established
                | smoltcp::socket::tcp::State::CloseWait
                | smoltcp::socket::tcp::State::FinWait1
                | smoltcp::socket::tcp::State::FinWait2
        )
    }

    #[inline]
    fn apply_close_action(socket: &mut smoltcp::socket::tcp::Socket, action: CloseAction) {
        match action.kind {
            DeferredTcpCloseKind::Reset => socket.abort(),
            DeferredTcpCloseKind::GracefulFin => {
                if action.abort_on_post_close_data {
                    socket.shutdown_recv();
                }
                socket.close();
            }
        }
    }

    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let local_endpoint = self.normalize_endpoint(local_endpoint, false)?;
        let v6_only = self
            .options
            .ipv6_only
            .load(core::sync::atomic::Ordering::Relaxed);
        match writer.take().expect("Tcp inner::Inner is None") {
            inner::Inner::Init(inner) => match inner.bind(
                local_endpoint,
                self.netns(),
                v6_only,
                self.device_binding.clone(),
            ) {
                Ok(bound) => {
                    // Linux inet6_bind() makes a concrete native IPv6 binding v6-only.
                    if local_endpoint.addr.version() == smoltcp::wire::IpVersion::Ipv6
                        && !local_endpoint.addr.is_unspecified()
                    {
                        self.options
                            .ipv6_only
                            .store(true, core::sync::atomic::Ordering::Relaxed);
                    }
                    if let inner::Init::Bound((ref bound, _, _)) = bound {
                        bound
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                    }
                    writer.replace(inner::Inner::Init(bound));
                    Ok(())
                }
                Err((inner, err)) => {
                    writer.replace(inner::Inner::Init(inner));
                    Err(err)
                }
            },
            any => {
                writer.replace(any);
                log::error!("TcpSocket::do_bind: not Init");
                Err(SystemError::EINVAL)
            }
        }
    }

    pub fn do_listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp inner::Inner is None");
        let (listening, err) = match inner {
            inner::Inner::Init(init) => {
                let listen_result = init.listen(
                    backlog,
                    self.netns(),
                    self.options
                        .ipv6_only
                        .load(core::sync::atomic::Ordering::Relaxed),
                    self.device_binding.clone(),
                );
                match listen_result {
                    Ok(listening) => {
                        // A logical listener remains live when all physical
                        // accept slots are occupied. Publish its receive domain
                        // for smoltcp's unmatched-SYN fallback.
                        //
                        // For INADDR_ANY listeners, listen sockets span multiple interfaces,
                        // so register on each unique interface.
                        let port = listening.get_name().port;
                        let me = self.self_ref.upgrade().unwrap();
                        let mut registered_ifaces: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
                        for b in &listening.inners {
                            let nic_id = b.iface().nic_id();
                            if !registered_ifaces.contains(&nic_id) {
                                b.iface().common().register_tcp_listener(
                                    listening.reservation.as_ref().unwrap().id,
                                    listening.domain,
                                    port,
                                    self.device_binding.ifindex() as u32,
                                );
                                b.iface().common().bind_socket(me.clone());
                                registered_ifaces.push(nic_id);
                            }
                        }
                        (inner::Inner::Listening(listening), None)
                    }
                    Err((init, err)) => (inner::Inner::Init(init), Some(err)),
                }
            }
            inner::Inner::Listening(mut listening) => {
                let err = listening.set_backlog(backlog).err();
                (inner::Inner::Listening(listening), err)
            }
            _ => (inner, Some(SystemError::EINVAL)),
        };
        writer.replace(listening);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn try_accept(&self) -> Result<(Arc<TcpSocket>, smoltcp::wire::IpEndpoint), SystemError> {
        // 主动推进协议栈：避免依赖后台 poll 线程，保证 accept 在无事件通知场景下也能前进。
        // For INADDR_ANY listeners, poll all interfaces that have listen sockets.
        let ifaces = {
            let reader = self.inner.read();
            if let Some(inner::Inner::Listening(listening)) = reader.as_ref() {
                let mut ifaces: alloc::vec::Vec<Arc<dyn crate::net::Iface>> =
                    alloc::vec::Vec::new();
                for b in &listening.inners {
                    let nic_id = b.iface().nic_id();
                    if !ifaces.iter().any(|iface| iface.nic_id() == nic_id) {
                        ifaces.push(b.iface().clone());
                    }
                }
                ifaces
            } else if let Some(iface) = reader.as_ref().and_then(|inner| inner.iface()).cloned() {
                alloc::vec![iface]
            } else {
                alloc::vec::Vec::new()
            }
        };
        for iface in ifaces {
            iface.poll();
        }

        match self
            .inner
            .write()
            .as_mut()
            .expect("Tcp inner::Inner is None")
        {
            inner::Inner::Listening(listening) => {
                let (socket, point) = listening.accept().map(|(stream, remote)| {
                    (
                        TcpSocket::new_established(
                            stream,
                            self.is_nonblock(),
                            self.netns(),
                            self.ip_version,
                        ),
                        remote,
                    )
                })?;
                socket.options.ipv6_only.store(
                    self.options
                        .ipv6_only
                        .load(core::sync::atomic::Ordering::Relaxed),
                    core::sync::atomic::Ordering::Relaxed,
                );
                {
                    let mut inner_guard = socket.inner.write();
                    if let Some(inner::Inner::Established(established)) = inner_guard.as_mut() {
                        established.iface().common().bind_socket(socket.clone());
                    }
                }

                Ok((socket, point))
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    // SHOULD refactor
    pub fn start_connect(
        &self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
    ) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let remote_endpoint = self.normalize_endpoint(remote_endpoint, true)?;
        let remote_endpoint =
            crate::net::socket::inet::common::normalize_unspecified_endpoint_to_loopback(
                remote_endpoint,
            );
        // Explicit-source and self-connect paths need the same device/route
        // validation as implicit binding, before changing the socket state.
        if let Some(inner::Inner::Init(inner::Init::Bound((_, local, _)))) = writer.as_ref() {
            if local.addr.version() == smoltcp::wire::IpVersion::Ipv4
                && !local.addr.is_unspecified()
            {
                let device = self.device_binding.resolve_iface(&self.netns)?;
                crate::net::route::resolve_ipv4_route(
                    &self.netns,
                    remote_endpoint.addr,
                    device.map(|iface| iface.nic_id() as u32),
                    Some(local.addr),
                )?;
            }
        }
        let inner = writer.take().expect("Tcp inner::Inner is None");
        let old_iface = inner.iface().cloned();
        let (init, result) = match inner {
            inner::Inner::Init(init) => {
                match init.connect(
                    remote_endpoint,
                    self.netns(),
                    self.self_ref.clone(),
                    self.ip_version,
                    self.device_binding.clone(),
                ) {
                    Ok(connecting) => (
                        inner::Inner::Connecting(connecting),
                        if self.is_nonblock() {
                            Err(SystemError::EINPROGRESS)
                        } else {
                            Ok(())
                        },
                    ),
                    Err((init, err)) => (inner::Inner::Init(init), Err(err)),
                }
            }
            inner::Inner::Connecting(connecting) => {
                // Check if the connection has already failed.
                if let Some(err) = connecting.failure_reason() {
                    let (new_inner, _) = connecting.into_result();
                    (new_inner, Err(err))
                } else if connecting.is_refused_consumed() {
                    let (new_inner, _) = connecting.into_result();
                    (new_inner, Err(SystemError::ECONNABORTED))
                } else if connecting.is_connected() {
                    let (new_inner, _) = connecting.into_result();
                    (new_inner, Ok(()))
                } else if self.is_nonblock() {
                    (
                        inner::Inner::Connecting(connecting),
                        Err(SystemError::EALREADY),
                    )
                } else {
                    (inner::Inner::Connecting(connecting), Ok(()))
                }
            }
            inner::Inner::Listening(inner) => {
                (inner::Inner::Listening(inner), Err(SystemError::EISCONN))
            }
            inner::Inner::Established(inner) => {
                (inner::Inner::Established(inner), Err(SystemError::EISCONN))
            }
            inner::Inner::Closed(_) => (inner, Err(SystemError::ENOTCONN)),
        };

        // Publish the state before releasing inner. Connection registration
        // retains its own cancellation-aware handoff; polling runs only after
        // dropping inner.
        let need_poll_progress = matches!(init, inner::Inner::Connecting(_));
        let registration_publisher = match &init {
            inner::Inner::Connecting(connecting) => Some(connecting.registration_publisher()),
            _ => None,
        };
        let maybe_iface = init.iface().cloned();
        writer.replace(init);

        // Current iface notifications release the bounds snapshot lock before
        // calling into a socket. Keep cleanup serialized with a subsequent
        // connect, which could otherwise publish a new registration here.
        if let Some(old_iface) = old_iface {
            if !maybe_iface
                .as_ref()
                .is_some_and(|iface| Arc::ptr_eq(iface, &old_iface))
            {
                old_iface
                    .common()
                    .unbind_socket(self.self_ref.upgrade().unwrap());
            }
        }
        drop(writer);

        // 关键语义：connect(2) 进入 Connecting 状态后，socket 必须能被网络轮询推进。
        if need_poll_progress && matches!(result, Ok(()) | Err(SystemError::EINPROGRESS)) {
            if let Some(iface) = maybe_iface {
                // log::debug!(
                //     "TcpSocket::start_connect: bind to iface nic_id={}, nonblock={}",
                //     iface.nic_id(),
                //     self.is_nonblock()
                // );
                registration_publisher
                    .expect("Connecting state must own a registration publisher")
                    .publish();

                if let Some(netns) = iface.common().net_namespace() {
                    netns.wakeup_poll_thread();
                }
                // 主动 poll 一次以尽快发出 SYN / 处理握手。
                iface.poll();
            }
        }

        result
    }

    pub fn check_connect(&self) -> Result<(), SystemError> {
        // 主动推进协议栈：connect 阻塞等待期间也要持续 poll，否则状态不会从 Connecting 前进。
        if let Some(iface) = self.stack_poll_iface_snapshot() {
            iface.poll();
        }

        self.update_events();
        let mut write_state = self.inner.write();
        let inner = write_state.take().expect("Tcp inner::Inner is None");
        let (replace, result) = match inner {
            inner::Inner::Connecting(conn) => conn.into_result(),
            inner::Inner::Established(es) => (inner::Inner::Established(es), Ok(())), // TODO check established
            _ => {
                log::warn!("TODO: connecting socket error options");
                (inner, Err(SystemError::EINVAL))
            } // TODO socket error options
        };
        write_state.replace(replace);
        result
    }

    pub fn do_shutdown(&self, _how: ShutdownBit) -> Result<(), SystemError> {
        let how = _how;
        if how.is_empty() {
            return Err(SystemError::EINVAL);
        }

        if how.contains(ShutdownBit::SHUT_WR) {
            if let Err(e) = self.flush_cork_buffer() {
                if e != SystemError::EAGAIN_OR_EWOULDBLOCK {
                    return Err(e);
                }
            }
        }

        let mut post_poll_iface: Option<Arc<dyn crate::net::Iface>> = None;
        let mut post_poll_rounds = 0usize;
        let mut post_notify_bound_sockets = false;

        // Linux/gVisor 语义：TIME_WAIT/Closed 的 stream socket 上 shutdown 应返回 ENOTCONN。
        // 但 Listening 和 Connecting 状态下的 shutdown 是允许的。
        // Serialize pending cork data, FIN and the shutdown bit with send's
        // enqueue path. A concurrent flusher may have returned early above.
        // Keep the same cork -> inner order as cork-flush completion.
        let cork_buf = self.cork_buf.lock();
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp inner::Inner is None");

        let (replace, record_bits) = match inner {
            inner::Inner::Established(established) => {
                let state = established.with(|socket| socket.state());
                if matches!(
                    state,
                    smoltcp::socket::tcp::State::TimeWait | smoltcp::socket::tcp::State::Closed
                ) {
                    writer.replace(inner::Inner::Established(established));
                    return Err(SystemError::ENOTCONN);
                }

                if how.contains(ShutdownBit::SHUT_RD) {
                    let queued = established.with(|socket| socket.recv_queue());
                    self.recv_shutdown.init(queued);
                }

                if how.contains(ShutdownBit::SHUT_WR) {
                    self.send_fin_deferred
                        .store(!cork_buf.is_empty(), core::sync::atomic::Ordering::Relaxed);
                    if cork_buf.is_empty() {
                        // smoltcp orders FIN after its queued data; waiting for
                        // ACKs here would unnecessarily delay shutdown.
                        established.with_mut(|socket| socket.close());
                    }
                    post_poll_rounds =
                        core::cmp::max(post_poll_rounds, TCP_ESTABLISHED_POST_POLL_ROUNDS);
                    post_poll_iface = Some(established.iface().clone());
                }

                // For Established stream sockets, shutdown affects both send/recv behavior.
                (inner::Inner::Established(established), how)
            }
            inner::Inner::Listening(mut listening) => {
                if how.contains(ShutdownBit::SHUT_RD) {
                    let original_listen_sockets = listening.inners.len();
                    let local = listening.get_name();
                    let reservation_id = listening.reservation.as_ref().unwrap().id;

                    // Unregister listen port and unbind socket from all unique interfaces.
                    // For INADDR_ANY listeners, listen sockets span multiple interfaces.
                    {
                        let me = self.self_ref.upgrade().unwrap();
                        let mut unregistered: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
                        for b in &listening.inners {
                            let nic_id = b.iface().nic_id();
                            if !unregistered.contains(&nic_id) {
                                b.iface().common().unregister_tcp_listener(reservation_id);
                                b.iface().common().unbind_socket(me.clone());
                                unregistered.push(nic_id);
                            }
                        }
                    }

                    for bound in &listening.inners {
                        bound.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                            socket.abort();
                        });
                    }

                    let keep = listening
                        .inners
                        .pop()
                        .expect("Listening socket must have at least one inner");
                    for bound in &listening.inners {
                        bound.release();
                    }

                    post_poll_rounds = core::cmp::max(
                        post_poll_rounds,
                        Self::listen_post_poll_rounds(original_listen_sockets),
                    );
                    post_notify_bound_sockets = true;
                    post_poll_iface = Some(keep.iface().clone());

                    // Linux: shutdown(SHUT_RD) on a listening socket stops listening.
                    // Do not record SHUT_RD bit here because recv() on an unconnected
                    // stream socket should not become EOF just due to this operation.
                    (
                        inner::Inner::Init(inner::Init::Bound((
                            keep,
                            local,
                            listening.reservation.take().unwrap(),
                        ))),
                        ShutdownBit::from_bits_truncate(0),
                    )
                } else {
                    (
                        inner::Inner::Listening(listening),
                        ShutdownBit::from_bits_truncate(0),
                    )
                }
            }
            inner::Inner::Connecting(connecting) => {
                if connecting.is_transport_established() {
                    let established = unsafe { connecting.into_established() };

                    if how.contains(ShutdownBit::SHUT_RD) {
                        let queued = established.with(|socket| socket.recv_queue());
                        self.recv_shutdown.init(queued);
                    }

                    if how.contains(ShutdownBit::SHUT_WR) {
                        self.send_fin_deferred
                            .store(!cork_buf.is_empty(), core::sync::atomic::Ordering::Relaxed);
                        if cork_buf.is_empty() {
                            established.with_mut(|socket| socket.close());
                        }
                        post_poll_rounds =
                            core::cmp::max(post_poll_rounds, TCP_ESTABLISHED_POST_POLL_ROUNDS);
                        post_poll_iface = Some(established.iface().clone());
                    }

                    (inner::Inner::Established(established), how)
                } else {
                    connecting.set_shutdown_reset();
                    connecting.with_mut(|socket| socket.abort());
                    post_poll_rounds =
                        core::cmp::max(post_poll_rounds, TCP_CONNECTING_ABORT_POST_POLL_ROUNDS);
                    post_poll_iface = Some(connecting.iface().clone());

                    // For still-connecting sockets, only SHUT_WR is meaningful for
                    // user-visible send() behavior (EPIPE). Recording SHUT_RD would
                    // incorrectly make recv() return EOF on an unconnected stream socket.
                    (
                        inner::Inner::Connecting(connecting),
                        if how.contains(ShutdownBit::SHUT_WR) {
                            ShutdownBit::SHUT_WR
                        } else {
                            ShutdownBit::from_bits_truncate(0)
                        },
                    )
                }
            }
            other => {
                writer.replace(other);
                return Err(SystemError::ENOTCONN);
            }
        };

        if !record_bits.is_empty() {
            self.shutdown.fetch_or(
                record_bits.bits() as usize,
                core::sync::atomic::Ordering::AcqRel,
            );
        }

        writer.replace(replace);
        drop(writer);
        drop(cork_buf);

        // 唤醒等待者（含 poll/epoll），让状态变化可见。
        if let Some(iface) = post_poll_iface {
            Self::kick_iface_after_tcp_state_change(
                &iface,
                post_poll_rounds,
                post_notify_bound_sockets,
            );
        }
        self.notify();
        Ok(())
    }

    pub fn close_socket(&self) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let Some(inner) = writer.take() else {
            log::warn!("TcpSocket::close: already closed, unexpected");
            return Ok(());
        };

        let mut post_poll_iface: Option<Arc<dyn crate::net::Iface>> = None;
        let mut post_poll_rounds = 0usize;
        let mut post_notify_bound_sockets = false;

        // close(fd) must not break in-flight syscalls that already hold a
        // reference to this socket object (gVisor ClosedWriteBlockingSocket).
        // So we do NOT leave self.inner as None; we always reinsert it below.
        //
        // For Listening sockets, unbind_socket must be done per-iface inside the
        // Listening match arm (INADDR_ANY spans multiple interfaces). For all other
        // states, inner.iface() returns the single owning iface.
        // Keep routed output published from notification unregistration until
        // close/abort output is either removed or owned by tcp_close_defer.
        // This is the close-side counterpart of Connecting's first-SYN guard.
        let _routing_publication = if !matches!(inner, inner::Inner::Listening(_)) {
            inner
                .iface()
                .cloned()
                .map(crate::net::socket::inet::common::RoutedSocketPublication::begin)
        } else {
            None
        };
        if !matches!(inner, inner::Inner::Listening(_)) {
            if let Some(iface) = inner.iface() {
                iface
                    .common()
                    .unbind_socket(self.self_ref.upgrade().unwrap());
            }
        }

        match inner {
            // complete connecting socket close logic
            inner::Inner::Connecting(conn) => {
                // Ensure we have the latest state from smoltcp
                let _ = conn.update_io_events(&self.pollee);

                if conn.failure_reason().is_some() {
                    conn.consume_error();
                    let (new_inner, _) = conn.into_result();
                    writer.replace(new_inner);
                } else if conn.is_refused_consumed() {
                    let (new_inner, _) = conn.into_result();
                    writer.replace(new_inner);
                } else {
                    let mut conn = unsafe { conn.into_established_after_unbind() };
                    let handle = conn.handle();
                    let local_port = conn.get_name().port;
                    let iface = conn.iface().clone();
                    let me: alloc::sync::Weak<dyn InetSocket> = self.self_ref.clone();
                    conn.with_mut(|socket| socket.abort());
                    let initial_state = conn.with(|socket| socket.state());
                    conn.release_port();
                    iface.common().defer_tcp_close(DeferredTcpCloseRequest {
                        handle,
                        local_port,
                        sock: me,
                        initial_state,
                        kind: DeferredTcpCloseKind::Reset,
                        reason: DeferredTcpCloseReason::ConnectingClose,
                        abort_on_post_close_data: false,
                    });
                    post_poll_rounds =
                        core::cmp::max(post_poll_rounds, TCP_CONNECTING_ABORT_POST_POLL_ROUNDS);
                    post_poll_iface = Some(iface);
                    writer.replace(inner::Inner::Established(conn));
                }
            }
            inner::Inner::Established(mut es) => {
                // A successful connect can still have a publisher racing from
                // start_connect(). Cancel its transferable lease before this
                // closed socket can be reinserted into iface bounds.
                es.cancel_connecting_registration();
                let handle = es.handle();
                let local_port = es.get_name().port;
                let iface = es.iface().clone();
                let me: alloc::sync::Weak<dyn InetSocket> = self.self_ref.clone();
                let close_action = self.decide_established_close(&es);
                es.with_mut(|socket| Self::apply_close_action(socket, close_action));
                let initial_state = es.with(|socket| socket.state());
                es.release_port();
                iface.common().defer_tcp_close(DeferredTcpCloseRequest {
                    handle,
                    local_port,
                    sock: me,
                    initial_state,
                    kind: close_action.kind,
                    reason: close_action.reason,
                    abort_on_post_close_data: close_action.abort_on_post_close_data,
                });
                post_poll_rounds =
                    core::cmp::max(post_poll_rounds, TCP_ESTABLISHED_CLOSE_POST_POLL_ROUNDS);
                post_poll_iface = Some(iface);
                writer.replace(inner::Inner::Established(es));
            }
            inner::Inner::Listening(mut ls) => {
                // Each backlog slot can already be in a state that emits FIN
                // when closed. Publish every owning stack before removing the
                // listener from `bounds`; balanced counters avoid allocation
                // in this infallible teardown path.
                for bound in &ls.inners {
                    bound.iface().common().begin_routed_socket_publication();
                }
                // close(listen_fd) should stop listening on the port.
                let original_listen_sockets = ls.inners.len();
                let reservation_id = ls.reservation.as_ref().unwrap().id;
                let post_close_iface = ls.inners.first().map(|b| b.iface().clone());
                // Unregister listen port and unbind socket from all unique interfaces.
                // For INADDR_ANY listeners, listen sockets span multiple interfaces,
                // so we must clean up each one.
                {
                    let me = self.self_ref.upgrade().unwrap();
                    let mut cleaned: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
                    for b in &ls.inners {
                        let nic_id = b.iface().nic_id();
                        if !cleaned.contains(&nic_id) {
                            b.iface().common().unregister_tcp_listener(reservation_id);
                            b.iface().common().unbind_socket(me.clone());
                            cleaned.push(nic_id);
                        }
                    }
                }
                ls.close();
                // IMPORTANT:
                // `ls.release()` 会把 Listening::inners 里的 handle 从 SocketSet 中 remove。
                // 由于 poll 路径可能已经快照了该 TcpSocket 的 Arc，并在 close_socket() 之后仍调用一次 notify，
                // 如果我们仍把 inner 维持在 Listening 状态，则 update_events() 会遍历 inners 并访问已失效 handle，
                // 导致 smoltcp panic: "handle does not refer to a valid socket"。
                //
                // 因此这里必须在 release 后把状态切到显式 Closed，确保后续 update_events 不再触达 SocketSet，
                // 同时语义上也更“优雅”。
                ls.release();
                for bound in &ls.inners {
                    bound.iface().common().finish_routed_socket_publication();
                }
                let ver = match ls.get_name().addr {
                    smoltcp::wire::IpAddress::Ipv6(_) => smoltcp::wire::IpVersion::Ipv6,
                    _ => smoltcp::wire::IpVersion::Ipv4,
                };
                writer.replace(inner::Inner::Closed(inner::Closed::new(ver)));
                post_poll_rounds = core::cmp::max(
                    post_poll_rounds,
                    Self::listen_post_poll_rounds(original_listen_sockets),
                );
                post_notify_bound_sockets = true;
                post_poll_iface = post_close_iface;
            }
            inner::Inner::Init(init) => {
                writer.replace(inner::Inner::Closed(init.close()));
            }
            inner::Inner::Closed(closed) => {
                // Already closed: keep the Closed state.
                writer.replace(inner::Inner::Closed(closed));
            }
        };
        drop(writer);
        if let Some(iface) = post_poll_iface {
            Self::kick_iface_after_tcp_state_change(
                &iface,
                post_poll_rounds,
                post_notify_bound_sockets,
            );
        }
        self.notify();
        Ok(())
    }
}
