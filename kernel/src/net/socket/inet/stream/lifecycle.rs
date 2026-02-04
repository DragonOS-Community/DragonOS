use crate::net::socket::common::ShutdownBit;
use crate::net::socket::inet::InetSocket;
use crate::net::socket::inet::Types;
use alloc::sync::Arc;
use system_error::SystemError;

use super::inner;
use super::TcpSocket;

impl TcpSocket {
    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        match writer.take().expect("Tcp inner::Inner is None") {
            inner::Inner::Init(inner) => {
                let bound = inner.bind(local_endpoint, self.netns())?;
                if let inner::Init::Bound((ref bound, _)) = bound {
                    bound
                        .iface()
                        .common()
                        .bind_socket(self.self_ref.upgrade().unwrap());
                }
                writer.replace(inner::Inner::Init(bound));
                Ok(())
            }
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
                let listen_result = init.listen(backlog);
                match listen_result {
                    Ok(listening) => {
                        // DragonOS backlog emulation: listener is represented by multiple
                        // smoltcp TCP sockets. When all LISTEN sockets are consumed,
                        // Linux commonly drops incoming SYN (no RST). To implement this
                        // without changing smoltcp semantics, register the active listen port
                        // in the iface common registry.
                        let port = listening.get_name().port;
                        if let Some(b) = listening.inners.first() {
                            b.iface().common().register_tcp_listen_port(port, backlog);
                        }
                        (inner::Inner::Listening(listening), None)
                    }
                    Err((init, err)) => (inner::Inner::Init(init), Some(err)),
                }
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
        if let Some(iface) = self
            .inner
            .read()
            .as_ref()
            .and_then(|inner| inner.iface())
            .cloned()
        {
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
        // log::debug!("TcpSocket::start_connect: remote={:?}", remote_endpoint);
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp inner::Inner is None");
        let (init, result) = match inner {
            inner::Inner::Init(init) => {
                // Linux-compatible self-connect: connect() to our own bound addr:port on the
                // same socket is allowed and results in a socket that can send/recv to itself.
                // smoltcp cannot model this with a single TCP socket instance, so we special-case
                // it into `Inner::SelfConnected`.
                match init {
                    inner::Init::Bound((bound, local)) if local == remote_endpoint => {
                        // Capture an effective queue capacity from the underlying socket's recv buffer.
                        let rx_cap = bound
                            .with::<smoltcp::socket::tcp::Socket, _, _>(|s| s.recv_capacity())
                            .clamp(1 << 20, super::constants::MAX_SOCKET_BUFFER);
                        (
                            inner::Inner::SelfConnected(inner::SelfConnected::new(
                                bound, local, rx_cap,
                            )),
                            Ok(()),
                        )
                    }
                    other => {
                        let conn_result = other.connect(remote_endpoint, self.netns());
                        match conn_result {
                            Ok(connecting) => (
                                inner::Inner::Connecting(connecting),
                                if !self.is_nonblock() {
                                    Ok(())
                                } else {
                                    Err(SystemError::EINPROGRESS)
                                },
                            ),
                            Err((init, err)) => (inner::Inner::Init(init), Err(err)),
                        }
                    }
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
            inner::Inner::SelfConnected(inner) => (
                inner::Inner::SelfConnected(inner),
                Err(SystemError::EISCONN),
            ),
            inner::Inner::Closed(_) => (inner, Err(SystemError::ENOTCONN)),
        };

        // 先落状态再做 iface 侧绑定，避免与 poll 路径形成锁顺序反转死锁：
        // - poll: bounds.read -> socket.notify -> socket.inner.read/write
        // - connect: socket.inner.write -> bounds.write  (会与上面互锁)
        // SelfConnected 不依赖协议栈推进，不应触发 iface.poll()
        let need_poll_progress = matches!(init, inner::Inner::Connecting(_));
        let maybe_iface = init.iface().cloned();
        writer.replace(init);
        drop(writer);

        // 关键语义：connect(2) 进入 Connecting 状态后，socket 必须能被网络轮询推进。
        if need_poll_progress && matches!(result, Ok(()) | Err(SystemError::EINPROGRESS)) {
            if let Some(iface) = maybe_iface {
                // log::debug!(
                //     "TcpSocket::start_connect: bind to iface nic_id={}, nonblock={}",
                //     iface.nic_id(),
                //     self.is_nonblock()
                // );
                let me = self
                    .self_ref
                    .upgrade()
                    .expect("TcpSocket::start_connect: self_ref upgrade failed");
                // 去重绑定：防止重复注册导致重复 notify/epoll 唤醒。
                iface.common().unbind_socket(me.clone());
                iface.common().bind_socket(me);

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
        if let Some(iface) = self
            .inner
            .read()
            .as_ref()
            .and_then(|inner| inner.iface())
            .cloned()
        {
            iface.poll();
        }

        self.update_events();
        let mut write_state = self.inner.write();
        let inner = write_state.take().expect("Tcp inner::Inner is None");
        let (replace, result) = match inner {
            inner::Inner::Connecting(conn) => conn.into_result(),
            inner::Inner::Established(es) => (inner::Inner::Established(es), Ok(())), // TODO check established
            inner::Inner::SelfConnected(sc) => (inner::Inner::SelfConnected(sc), Ok(())),
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
                if e == SystemError::EAGAIN_OR_EWOULDBLOCK {
                    // Defer FIN until cork-buffered bytes are flushed into the TCP stack.
                    self.send_fin_deferred
                        .store(true, core::sync::atomic::Ordering::Relaxed);
                } else {
                    return Err(e);
                }
            }
        }

        let mut post_poll_iface: Option<Arc<dyn crate::net::Iface>> = None;
        let mut post_poll_rounds: usize = 0;

        // Linux/gVisor 语义：TIME_WAIT/Closed 的 stream socket 上 shutdown 应返回 ENOTCONN。
        // 但 Listening 和 Connecting 状态下的 shutdown 是允许的。
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
                    let pending = established.with(|socket| socket.send_queue());
                    if pending > 0 {
                        // Defer FIN until all queued data has been sent.
                        self.send_fin_deferred
                            .store(true, core::sync::atomic::Ordering::Relaxed);
                    } else if self
                        .send_fin_deferred
                        .load(core::sync::atomic::Ordering::Relaxed)
                    {
                        // FIN will be sent once deferred bytes are fully flushed.
                    } else {
                        established.with_mut(|socket| socket.close());
                    }
                    post_poll_rounds = core::cmp::max(post_poll_rounds, 8);
                    post_poll_iface = Some(established.iface().clone());
                }

                // For Established stream sockets, shutdown affects both send/recv behavior.
                (inner::Inner::Established(established), how)
            }
            inner::Inner::Listening(mut listening) => {
                if how.contains(ShutdownBit::SHUT_RD) {
                    let original_listen_sockets = listening.inners.len();
                    let local = listening.get_name();
                    let port = local.port;

                    if let Some(b) = listening.inners.first() {
                        b.iface().common().unregister_tcp_listen_port(port);
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
                        original_listen_sockets.saturating_mul(8).clamp(128, 8192),
                    );
                    post_poll_iface = Some(keep.iface().clone());

                    // Linux: shutdown(SHUT_RD) on a listening socket stops listening.
                    // Do not record SHUT_RD bit here because recv() on an unconnected
                    // stream socket should not become EOF just due to this operation.
                    (
                        inner::Inner::Init(inner::Init::Bound((keep, local))),
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
                if how.contains(ShutdownBit::SHUT_RD) {
                    connecting.with_mut(|socket| {
                        socket.abort();
                    });
                    post_poll_rounds = core::cmp::max(post_poll_rounds, 128);
                    post_poll_iface = Some(connecting.iface().clone());
                }

                if how.contains(ShutdownBit::SHUT_WR) {
                    connecting.with_mut(|socket| socket.close());
                    post_poll_rounds = core::cmp::max(post_poll_rounds, 8);
                    post_poll_iface = Some(connecting.iface().clone());
                }

                // For Connecting sockets, only SHUT_WR is meaningful for user-visible
                // send() behavior (EPIPE). Recording SHUT_RD would incorrectly make
                // recv() return EOF on an unconnected stream socket.
                (
                    inner::Inner::Connecting(connecting),
                    if how.contains(ShutdownBit::SHUT_WR) {
                        ShutdownBit::SHUT_WR
                    } else {
                        ShutdownBit::from_bits_truncate(0)
                    },
                )
            }
            inner::Inner::SelfConnected(sc) => {
                // SelfConnected: shutdown affects only the user-visible data path.
                // - SHUT_WR: subsequent send() returns EPIPE; recv() returns EOF once queue drains.
                // - SHUT_RD: subsequent recv() returns 0.
                // No smoltcp close/abort is needed.
                if how.contains(ShutdownBit::SHUT_RD) {
                    let queued = sc.recv_queue();
                    self.recv_shutdown.init(queued);
                }
                (inner::Inner::SelfConnected(sc), how)
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

        // 唤醒等待者（含 poll/epoll），让状态变化可见。
        if let Some(iface) = post_poll_iface {
            if let Some(netns) = iface.common().net_namespace() {
                netns.wakeup_poll_thread();
            }
            for _ in 0..post_poll_rounds {
                iface.poll();
            }
            // After shutdown, explicitly notify all bound sockets on this interface.
            // This ensures that client sockets waiting for connection completion
            // are woken up even if the last poll() didn't detect state changes
            // (e.g., RST was already sent and received in earlier polls).
            iface.common().notify_all_bound_sockets();
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

        // close(fd) must not break in-flight syscalls that already hold a
        // reference to this socket object (gVisor ClosedWriteBlockingSocket).
        // So we do NOT leave self.inner as None; we always reinsert it below.
        if let Some(iface) = inner.iface() {
            iface
                .common()
                .unbind_socket(self.self_ref.upgrade().unwrap());
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
                } else {
                    let conn = unsafe { conn.into_established() };
                    let handle = conn.handle();
                    let local_port = conn.get_name().port;
                    let iface = conn.iface().clone();
                    let me: alloc::sync::Weak<dyn InetSocket> = self.self_ref.clone();
                    conn.close();
                    if conn.owns_port() {
                        iface.port_manager().unbind_port(Types::Tcp, local_port);
                    }
                    iface.common().defer_tcp_close(handle, local_port, me);
                    writer.replace(inner::Inner::Established(conn));
                }
            }
            inner::Inner::Established(es) => {
                let handle = es.handle();
                let local_port = es.get_name().port;
                let iface = es.iface().clone();
                let me: alloc::sync::Weak<dyn InetSocket> = self.self_ref.clone();
                let linger_abort = self
                    .linger_onoff()
                    .load(core::sync::atomic::Ordering::Relaxed)
                    != 0
                    && self
                        .linger_linger()
                        .load(core::sync::atomic::Ordering::Relaxed)
                        == 0;
                let unread = es.with(|socket| socket.recv_queue());
                if linger_abort || unread > 0 {
                    es.with_mut(|socket| socket.abort());
                    es.iface().poll();
                } else {
                    es.close();
                }
                if es.owns_port() {
                    iface.port_manager().unbind_port(Types::Tcp, local_port);
                }
                iface.common().defer_tcp_close(handle, local_port, me);
                writer.replace(inner::Inner::Established(es));
            }
            inner::Inner::SelfConnected(sc) => {
                // Release the bound handle and switch to explicit Closed to avoid stale handle access.
                let ver = match sc.get_name().addr {
                    smoltcp::wire::IpAddress::Ipv6(_) => smoltcp::wire::IpVersion::Ipv6,
                    _ => smoltcp::wire::IpVersion::Ipv4,
                };
                let port = sc.get_name().port;
                let iface = sc.iface().clone();
                sc.release();
                iface.port_manager().unbind_port(Types::Tcp, port);
                writer.replace(inner::Inner::Closed(inner::Closed::new(ver)));
            }
            inner::Inner::Listening(mut ls) => {
                // close(listen_fd) should stop listening on the port.
                let port = ls.get_name().port;
                if let Some(b) = ls.inners.first() {
                    b.iface().common().unregister_tcp_listen_port(port);
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
                let ver = match ls.get_name().addr {
                    smoltcp::wire::IpAddress::Ipv6(_) => smoltcp::wire::IpVersion::Ipv6,
                    _ => smoltcp::wire::IpVersion::Ipv4,
                };
                writer.replace(inner::Inner::Closed(inner::Closed::new(ver)));
            }
            inner::Inner::Init(init) => {
                init.close();
                writer.replace(inner::Inner::Init(init));
            }
            inner::Inner::Closed(closed) => {
                // Already closed: keep the Closed state.
                writer.replace(inner::Inner::Closed(closed));
            }
        };
        drop(writer);
        self.notify();
        Ok(())
    }
}
