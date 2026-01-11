use system_error::SystemError;

use crate::net::socket::inet::InetSocket;
use crate::net::socket::PMSG;

use super::inner;
use super::TcpSocket;

#[inline]
fn discard_recv_queue(socket: &mut smoltcp::socket::tcp::Socket) {
    while socket.can_recv() {
        match socket.recv(|data| (data.len(), data.len())) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

impl TcpSocket {
    fn recv_established(
        &self,
        socket: &mut smoltcp::socket::tcp::Socket,
        buf: &mut [u8],
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let mut current_buf = buf;
        let is_recv_shutdown = self.is_recv_shutdown();
        if is_recv_shutdown {
            let remaining = self.recv_shutdown.remaining_limit();
            if remaining == 0 {
                discard_recv_queue(socket);
                return Ok(0);
            }
            let cap = core::cmp::min(current_buf.len(), remaining);
            current_buf = &mut current_buf[..cap];
        }

        if !socket.can_recv() {
            // Linux 语义：对端已关闭写端(收到 FIN)且本端已读完数据时，recv 返回 0。
            // 如果状态表明已收到 FIN，即使 buffer 为空也应返回 0 (EOF)。
            let state = socket.state();
            if matches!(
                state,
                smoltcp::socket::tcp::State::CloseWait
                    | smoltcp::socket::tcp::State::LastAck
                    | smoltcp::socket::tcp::State::Closing
                    | smoltcp::socket::tcp::State::TimeWait
                    | smoltcp::socket::tcp::State::Closed
            ) {
                return Ok(0);
            }

            if !socket.may_recv() {
                return Ok(0);
            }
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        // gVisor tcp_socket.cc MsgTrunc* tests: for TCP stream, MSG_TRUNC means
        // "report the length but don't copy payload into userspace".
        // - Without MSG_PEEK: also consume (discard) the bytes.
        // - With MSG_PEEK: do not consume.
        if flags.contains(PMSG::TRUNC) {
            if flags.contains(PMSG::PEEK) {
                let queued = socket.recv_queue();
                return Ok(core::cmp::min(current_buf.len(), queued));
            }

            let mut total = 0usize;
            while total < current_buf.len() {
                if !socket.can_recv() {
                    break;
                }

                let want = current_buf.len() - total;
                let got = match socket.recv(|data| {
                    let take = core::cmp::min(want, data.len());
                    // Discard without copying.
                    (take, take)
                }) {
                    Ok(n) => n,
                    Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                        return Err(SystemError::ENOTCONN);
                    }
                    Err(smoltcp::socket::tcp::RecvError::Finished) => {
                        return Ok(total);
                    }
                };

                if got == 0 {
                    break;
                }
                total += got;
            }

            if total == 0 {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            if is_recv_shutdown && self.recv_shutdown.record_read(total) {
                discard_recv_queue(socket);
            }
            return Ok(total);
        }

        if flags.contains(PMSG::PEEK) {
            return match socket.peek_slice(current_buf) {
                Ok(size) => Ok(size),
                Err(smoltcp::socket::tcp::RecvError::InvalidState) => Err(SystemError::ENOTCONN),
                Err(smoltcp::socket::tcp::RecvError::Finished) => Ok(0),
            };
        }

        // smoltcp::tcp::Socket::recv_slice() 只会出队一段“连续”的 rx buffer。
        // 对于环形缓冲区发生 wrap 的情况，即使队列里有更多数据也可能只读到一部分。
        // Linux 的 stream socket 行为：一次 recv 尽量返回当前已到达的所有数据(直到用户缓冲区满)。
        let mut total = 0usize;
        while total < current_buf.len() {
            if !socket.can_recv() {
                break;
            }

            let want = current_buf.len() - total;
            let got = match socket.recv(|data| {
                let take = core::cmp::min(want, data.len());
                if take > 0 {
                    current_buf[total..total + take].copy_from_slice(&data[..take]);
                }
                (take, take)
            }) {
                Ok(n) => n,
                Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                    return Err(SystemError::ENOTCONN);
                }
                Err(smoltcp::socket::tcp::RecvError::Finished) => {
                    // FIN 已到达。
                    // 如果这次已读到部分数据，先把数据返回；否则返回 0 表示 EOF。
                    return Ok(total);
                }
            };

            if got == 0 {
                break;
            }
            total += got;
        }

        if total == 0 {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        if is_recv_shutdown && self.recv_shutdown.record_read(total) {
            discard_recv_queue(socket);
        }
        Ok(total)
    }

    pub(super) fn try_recv_with_flags(
        &self,
        buf: &mut [u8],
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let mut total_read = 0;

        loop {
            // SelfConnected does not rely on protocol-stack progress; avoid calling iface.poll()
            // here to prevent hangs when running the whole syscall test suite.
            let skip_iface_poll = matches!(
                self.inner.read().as_ref(),
                Some(inner::Inner::SelfConnected(_))
            );
            if !skip_iface_poll {
                if let Some(iface) = self
                    .inner
                    .read()
                    .as_ref()
                    .and_then(|inner| inner.iface())
                    .cloned()
                {
                    iface.poll();
                }
            }

            let iter_result = match self
                .inner
                .read()
                .as_ref()
                .expect("Tcp inner::Inner is None")
            {
                inner::Inner::Established(established) => established.with_mut(|socket| {
                    self.recv_established(socket, &mut buf[total_read..], flags)
                }),
                inner::Inner::SelfConnected(sc) => {
                    // Self-connect: data path is a local queue.
                    let mut current_buf = &mut buf[total_read..];
                    if self.is_recv_shutdown() {
                        let remaining = self.recv_shutdown.remaining_limit();
                        if remaining == 0 {
                            sc.discard_all();
                            return Ok(0);
                        }
                        let cap = core::cmp::min(current_buf.len(), remaining);
                        current_buf = &mut current_buf[..cap];
                    }
                    let peek = flags.contains(PMSG::PEEK);
                    let trunc = flags.contains(PMSG::TRUNC);
                    let send_shutdown = self.is_send_shutdown();
                    let n = sc.recv_into(current_buf, peek, trunc, send_shutdown)?;
                    if self.is_recv_shutdown() && !peek && self.recv_shutdown.record_read(n) {
                        sc.discard_all();
                    }
                    Ok(n)
                }
                inner::Inner::Connecting(connecting) => {
                    if let Some(err) = connecting.failure_reason() {
                        connecting.consume_error();
                        return Err(err);
                    }
                    if connecting.is_refused_consumed() {
                        return Ok(0);
                    }
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                }
                inner::Inner::Init(_) | inner::Inner::Closed(_) => Err(SystemError::ENOTCONN),
                _ => Err(SystemError::EINVAL),
            };

            match iter_result {
                Ok(n) => {
                    // For PEEK, we don't loop/accumulate because we are not consuming.
                    // Also for TRUNC+PEEK.
                    if flags.contains(PMSG::PEEK) {
                        return Ok(n);
                    }

                    total_read += n;

                    if n == 0 {
                        // EOF
                        break;
                    }

                    if total_read == buf.len() {
                        // Buffer full
                        break;
                    }

                    // We read some data, but buffer not full.
                    // Loop again to poll and see if more data arrived in NIC queue.
                }
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    if total_read > 0 {
                        break;
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                Err(e) => return Err(e),
            }
        }

        // For self-connect, consuming bytes frees space for senders waiting on EPOLLOUT.
        // Wake waiters and refresh pollee after we actually consumed data.
        if total_read > 0 && !flags.contains(PMSG::PEEK) {
            self.notify();
        }

        if let Some(iface) = self
            .inner
            .read()
            .as_ref()
            .and_then(|inner| inner.iface())
            .cloned()
        {
            // SelfConnected does not need iface.poll(); keep this call for real TCP sockets only.
            if !matches!(
                self.inner.read().as_ref(),
                Some(inner::Inner::SelfConnected(_))
            ) {
                iface.poll();
            }
        }

        Ok(total_read)
    }

    pub fn try_send(&self, buf: &[u8]) -> Result<usize, SystemError> {
        if buf.is_empty() {
            // Linux 语义：对 SOCK_STREAM，写入 0 字节应当立刻成功返回 0，且不阻塞。
            return Ok(0);
        }
        if self.is_send_shutdown() {
            return Err(SystemError::EPIPE);
        }
        if self
            .options
            .tcp_cork
            .load(core::sync::atomic::Ordering::Relaxed)
        {
            let mss = self
                .tcp_max_seg()
                .load(core::sync::atomic::Ordering::Relaxed);
            let cap = self
                .send_buf_size()
                .load(core::sync::atomic::Ordering::Relaxed);
            let mut cork_buf = self.cork_buf.lock();
            let free = cap.saturating_sub(cork_buf.len());
            if free == 0 {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let n = core::cmp::min(free, buf.len());
            cork_buf.extend_from_slice(&buf[..n]);
            if cork_buf.len() >= mss {
                drop(cork_buf);
                if let Err(e) = self.flush_cork_buffer() {
                    if e != SystemError::EAGAIN_OR_EWOULDBLOCK {
                        return Err(e);
                    }
                }
            }
            return Ok(n);
        }
        self.try_send_direct(buf)
    }

    pub(crate) fn flush_cork_buffer(&self) -> Result<(), SystemError> {
        loop {
            let cork_buf = self.cork_buf.lock();
            if cork_buf.is_empty() {
                return Ok(());
            }
            let data = cork_buf.clone();
            drop(cork_buf);

            let to_send = data.len();
            let sent = self.try_send_direct(data.as_slice())?;

            if sent == 0 {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            let mut cork_buf = self.cork_buf.lock();
            cork_buf.drain(..core::cmp::min(sent, to_send));
        }
    }

    fn try_send_direct(&self, buf: &[u8]) -> Result<usize, SystemError> {
        // Self-connect fast path: avoid smoltcp and iface polling.
        {
            let inner_guard = self.inner.read();
            if let Some(inner::Inner::SelfConnected(sc)) = inner_guard.as_ref() {
                let n = sc.send_slice(buf, self.is_send_shutdown())?;
                // Wake reader (same fd in another thread) and refresh events.
                self.notify();
                return Ok(n);
            }
        }
        // TODO: add nonblock check of connecting socket
        //
        // IMPORTANT: to avoid "all sleepers, no pollers" stalls on loopback (gVisor BlockingLargeSend),
        // we must ensure the protocol stack is progressed:
        // - poll BEFORE sending: to drain acks/advance state and make more send capacity available
        // - poll AFTER sending: to actually transmit queued segments and process immediate loopback delivery
        // Additionally, wake the netns poll thread so timers/retransmits can progress even if callers sleep.
        let maybe_iface = self
            .inner
            .read()
            .as_ref()
            .and_then(|inner| inner.iface())
            .cloned();
        if let Some(iface) = maybe_iface.as_ref() {
            if let Some(netns) = iface.common().net_namespace() {
                netns.wakeup_poll_thread();
            }
            // Loopback / fast-path correctness:
            // Poll once may only enqueue TX (or only process RX) without completing the
            // loopback roundtrip (TX->RX->ACK). If we return EAGAIN too early here,
            // acks processed shortly afterwards can free send buffer and make POLLOUT
            // appear spuriously (gVisor PollWithFullBufferBlocks).
            super::poll_util::poll_iface_until_quiescent(iface.as_ref());
        }

        // Fast path: Established.
        // NOTE: do not early-return while holding any lock; we may need to poll after send.
        let mut result: Option<Result<usize, SystemError>> = None;
        {
            let inner_guard = self.inner.read();
            if let Some(inner::Inner::Established(est)) = inner_guard.as_ref() {
                result = Some(est.send_slice(buf));
            }
        }
        if let Some(ret) = result {
            if let Some(iface) = maybe_iface.as_ref() {
                if let Some(netns) = iface.common().net_namespace() {
                    netns.wakeup_poll_thread();
                }
                super::poll_util::poll_iface_until_quiescent(iface.as_ref());
            }
            return ret;
        }

        // Handle transition from Connecting to Established
        let mut writer = self.inner.write();
        if let Some(inner) = writer.take() {
            let ret = match inner {
                inner::Inner::Connecting(conn) => {
                    let (new_inner, res) = conn.into_result();
                    match new_inner {
                        inner::Inner::Established(est) => {
                            let r = est.send_slice(buf);
                            writer.replace(inner::Inner::Established(est));
                            r
                        }
                        other => {
                            writer.replace(other);
                            // If connection failed, return error (EPIPE or EAGAIN if still connecting)
                            match res {
                                Ok(_) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK), // Should be Established if Ok
                                Err(e) => Err(e),
                            }
                        }
                    }
                }
                inner::Inner::Established(est) => {
                    let r = est.send_slice(buf);
                    writer.replace(inner::Inner::Established(est));
                    r
                }
                other => {
                    writer.replace(other);
                    Err(SystemError::EPIPE)
                }
            };

            // Drop lock before polling to avoid lock-order inversion with iface.poll()->notify().
            drop(writer);
            if let Some(iface) = maybe_iface.as_ref() {
                if let Some(netns) = iface.common().net_namespace() {
                    netns.wakeup_poll_thread();
                }
                super::poll_util::poll_iface_until_quiescent(iface.as_ref());
            }
            return ret;
        }

        Err(SystemError::ENOTCONN)
    }
}
