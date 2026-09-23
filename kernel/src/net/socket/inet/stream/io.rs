use alloc::boxed::Box;
use alloc::vec;

use system_error::SystemError;

use crate::exception::workqueue::{schedule_work, Work};
use crate::net::socket::inet::InetSocket;
use crate::net::socket::PMSG;
use crate::syscall::user_buffer::UserBuffer;
use crate::time::timer::{next_n_us_timer_jiffies, Timer, TimerFunction};

use alloc::sync::Weak;

use super::constants;
use super::inner;
use super::TcpSocket;

const USER_RECV_STAGING_SIZE: usize = 64 * 1024;

impl TcpSocket {
    fn maybe_schedule_cork_timeout(&self) {
        if !self
            .options
            .tcp_cork
            .load(core::sync::atomic::Ordering::Relaxed)
        {
            return;
        }

        if self.cork_buf.lock().is_empty() {
            return;
        }

        if self
            .cork_timer_active
            .compare_exchange(
                false,
                true,
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }

        let timer = Timer::new(
            Box::new(CorkFlushTimer {
                socket: self.self_ref.clone(),
            }),
            next_n_us_timer_jiffies(constants::TCP_CORK_FLUSH_TIMEOUT_US),
        );
        timer.activate();
    }

    fn handle_cork_timeout(&self) {
        self.cork_timer_active
            .store(false, core::sync::atomic::Ordering::Release);

        if self.cork_buf.lock().is_empty() {
            return;
        }

        if let Err(e) = self.flush_cork_buffer() {
            if e == SystemError::EAGAIN_OR_EWOULDBLOCK {
                self.maybe_schedule_cork_timeout();
            }
        }
    }

    // Called with the empty cork buffer locked. Shutdown and enqueue use the
    // same cork -> inner order, so no data can be published behind this FIN.
    fn maybe_complete_shutdown_wr_fin(&self) {
        if !self.is_send_shutdown() {
            return;
        }
        let mut writer = self.inner.write();
        if let Some(inner::Inner::Established(established)) = writer.as_mut() {
            if self
                .send_fin_deferred
                .swap(false, core::sync::atomic::Ordering::Relaxed)
            {
                established.with_mut(|socket| socket.close());
            }
        }
    }

    fn recv_reset_result(
        socket: &mut smoltcp::socket::tcp::Socket,
        report_error: bool,
    ) -> Result<usize, SystemError> {
        // A short successful read must leave the error for the next syscall.
        if !report_error && socket.reset_state().is_some() {
            return Ok(0);
        }
        socket
            .take_reset()
            .map(inner::reset_error)
            .map_or(Ok(0), Err)
    }

    fn recv_established(
        &self,
        socket: &mut smoltcp::socket::tcp::Socket,
        buf: &mut [u8],
        flags: PMSG,
        report_error: bool,
    ) -> Result<usize, SystemError> {
        let current_buf = buf;

        if !socket.can_recv() {
            if !socket.may_recv() {
                return match socket.recv(|_data| (0usize, ())) {
                    Ok(()) => Ok(0),
                    Err(smoltcp::socket::tcp::RecvError::Finished) => Ok(0),
                    Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                        Self::recv_reset_result(socket, report_error)
                    }
                };
            }
            // Linux drains queued bytes before observing local SHUT_RD. It
            // does not freeze the readable length when shutdown is requested.
            return if self.is_recv_shutdown() {
                Ok(0)
            } else {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            };
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
            return Ok(total);
        }

        if flags.contains(PMSG::PEEK) {
            return match socket.peek_slice(current_buf) {
                Ok(size) => Ok(size),
                Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                    Self::recv_reset_result(socket, report_error)
                }
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
        Ok(total)
    }

    fn recv_established_to_user(
        &self,
        established: &inner::Established,
        user_buffer: &mut UserBuffer<'_>,
        offset: usize,
        report_error: bool,
    ) -> Result<usize, SystemError> {
        if offset > user_buffer.len() {
            return Err(SystemError::EINVAL);
        }

        let user_remaining = user_buffer.len() - offset;

        if user_remaining == 0 {
            return Ok(0);
        }

        // Avoid allocating the staging buffer for the common nonblocking EAGAIN path.
        let readable = established.with_mut(|socket| {
            if socket.can_recv() {
                return Ok(true);
            }
            if socket.may_recv() {
                return if self.is_recv_shutdown() {
                    Ok(false)
                } else {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                };
            }
            match socket.recv(|_data| (0usize, ())) {
                Ok(()) | Err(smoltcp::socket::tcp::RecvError::Finished) => Ok(false),
                Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                    Self::recv_reset_result(socket, report_error).map(|_| false)
                }
            }
        })?;
        if !readable {
            return Ok(0);
        }

        // User pages may fault and sleep. Copy through a bounded kernel buffer so the
        // interface-wide SocketSet lock is never held while touching userspace.
        let mut staging = vec![0u8; core::cmp::min(user_remaining, USER_RECV_STAGING_SIZE)];

        let mut total = 0usize;
        while total < user_remaining {
            let want = user_remaining - total;
            let staged = established.with_mut(|socket| {
                if !socket.can_recv() {
                    return 0;
                }
                match socket.peek(core::cmp::min(want, staging.len())) {
                    Ok(data) => {
                        let size = data.len();
                        staging[..size].copy_from_slice(data);
                        size
                    }
                    Err(_) => 0,
                }
            });
            if staged == 0 {
                break;
            }

            if let Err(error) = user_buffer.write_to_user(offset + total, &staging[..staged]) {
                if error == SystemError::EFAULT && total > 0 {
                    break;
                }
                return Err(error);
            }

            // Ingress can append bytes while the SocketSet lock is dropped; a terminal state
            // transition can instead clear the queue. recv_lock excludes local consumers, so the
            // head cannot otherwise move. Require the snapshotted contiguous length before
            // consuming; if the queue was cleared, the copied bytes linearize immediately before
            // that terminal transition.
            let committed = established.with_mut(|socket| {
                socket
                    .recv(|data| {
                        if data.len() >= staged {
                            (staged, true)
                        } else {
                            (0, false)
                        }
                    })
                    .unwrap_or_default()
            });

            total += staged;
            if !committed {
                break;
            }
        }

        if total == 0 {
            // The stack may have processed FIN/RST after the allocation-time readiness probe.
            // Re-evaluate the terminal state instead of turning EOF into a spurious EAGAIN.
            return established.with_mut(|socket| {
                if socket.can_recv() {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                if socket.may_recv() {
                    return if self.is_recv_shutdown() {
                        Ok(0)
                    } else {
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    };
                }
                match socket.recv(|_data| (0usize, ())) {
                    Ok(()) | Err(smoltcp::socket::tcp::RecvError::Finished) => Ok(0),
                    Err(smoltcp::socket::tcp::RecvError::InvalidState) => {
                        Self::recv_reset_result(socket, report_error)
                    }
                }
            });
        }
        Ok(total)
    }

    fn finish_recv_progress(&self, consumed: bool) {
        if consumed {
            self.notify();
        }

        if let Some(iface) = self.stack_poll_snapshot() {
            // After a successful TCP recv() we may have just freed a significant portion of the
            // receive window. On loopback/blocking-large-send paths, sender progress depends on
            // promptly turning that freed window into ACK/window-update processing and sender-side
            // wakeups. A single poll is not always enough to complete the roundtrip, so mirror the
            // send path and drive the stack until quiescent.
            if let Some(netns) = iface.net_namespace() {
                netns.wakeup_tcp_poll();
            }
            super::poll_util::poll_stack_batch(iface.as_ref());
        }
    }

    pub(super) fn try_recv_with_flags(
        &self,
        buf: &mut [u8],
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let recv_guard = self.recv_lock.lock();
        let mut total_read = 0;

        loop {
            if let Some(iface) = self.stack_poll_snapshot() {
                if let Some(netns) = iface.net_namespace() {
                    netns.wakeup_tcp_poll();
                }
                super::poll_util::poll_stack_batch(iface.as_ref());
            }

            self.update_events();
            let iter_result = match self
                .inner
                .read()
                .as_ref()
                .expect("Tcp inner::Inner is None")
            {
                inner::Inner::Established(established) => established.with_mut(|socket| {
                    self.recv_established(socket, &mut buf[total_read..], flags, total_read == 0)
                }),
                inner::Inner::Connecting(connecting) => {
                    if let Some(err) = connecting.take_connect_error() {
                        Err(err)
                    } else if connecting.is_connected() {
                        continue;
                    } else if connecting.is_refused_consumed() {
                        Ok(0)
                    } else {
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    }
                }
                inner::Inner::Init(_) | inner::Inner::Closed(_) => Err(SystemError::ENOTCONN),
                _ => Err(SystemError::EINVAL),
            };

            // The inner and SocketSet guards are gone before refreshing the
            // cached readiness after a possible error consumption.
            self.update_events();
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
        drop(recv_guard);
        self.finish_recv_progress(total_read > 0 && !flags.contains(PMSG::PEEK));

        Ok(total_read)
    }

    fn try_recv_to_user_buffer(
        &self,
        user_buffer: &mut UserBuffer<'_>,
        offset: usize,
    ) -> Result<usize, SystemError> {
        let recv_guard = self.recv_lock.lock();
        let mut total_read = offset;

        loop {
            if let Some(iface) = self.stack_poll_snapshot() {
                if let Some(netns) = iface.net_namespace() {
                    netns.wakeup_tcp_poll();
                }
                super::poll_util::poll_stack_batch(iface.as_ref());
            }

            self.update_events();
            let iter_result = match self
                .inner
                .read()
                .as_ref()
                .expect("Tcp inner::Inner is None")
            {
                inner::Inner::Established(established) => self.recv_established_to_user(
                    established,
                    user_buffer,
                    total_read,
                    total_read == 0,
                ),
                inner::Inner::Connecting(connecting) => {
                    if let Some(err) = connecting.take_connect_error() {
                        Err(err)
                    } else if connecting.is_connected() {
                        continue;
                    } else if connecting.is_refused_consumed() {
                        Ok(0)
                    } else {
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    }
                }
                inner::Inner::Init(_) | inner::Inner::Closed(_) => Err(SystemError::ENOTCONN),
                _ => Err(SystemError::EINVAL),
            };

            self.update_events();
            match iter_result {
                Ok(n) => {
                    total_read += n;

                    if n == 0 || total_read == user_buffer.len() {
                        break;
                    }
                }
                Err(e) => {
                    if total_read > offset {
                        break;
                    }
                    return Err(e);
                }
            }
        }

        drop(recv_guard);
        self.finish_recv_progress(total_read > offset);
        Ok(total_read - offset)
    }

    pub(super) fn try_read_to_user_buffer(
        &self,
        user_buffer: &mut UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        self.try_recv_to_user_buffer(user_buffer, 0)
    }

    pub(super) fn recv_to_user_buffer_impl(
        &self,
        user_buffer: &mut UserBuffer<'_>,
        nonblock: bool,
        waitall: bool,
    ) -> Result<usize, SystemError> {
        if nonblock {
            return self.try_read_to_user_buffer(user_buffer);
        }

        let mut total_read = 0usize;
        loop {
            match self.try_recv_to_user_buffer(user_buffer, total_read) {
                Ok(n) => {
                    total_read += n;
                    if n == 0 || total_read == user_buffer.len() || !waitall {
                        return Ok(total_read);
                    }
                }
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    if let Some(iface) = self.stack_poll_snapshot() {
                        super::poll_util::poll_stack_batch(iface.as_ref());
                    }
                    let events = self.check_io_event();
                    if events.intersects(
                        crate::filesystem::epoll::EPollEventType::EPOLLIN
                            | crate::filesystem::epoll::EPollEventType::EPOLLHUP
                            | crate::filesystem::epoll::EPollEventType::EPOLLRDHUP
                            | crate::filesystem::epoll::EPollEventType::EPOLLERR,
                    ) {
                        continue;
                    }

                    let wait_ret = self.wait_queue.wait_event_io_interruptible_timeout(
                        || {
                            self.check_io_event().intersects(
                                crate::filesystem::epoll::EPollEventType::EPOLLIN
                                    | crate::filesystem::epoll::EPollEventType::EPOLLHUP
                                    | crate::filesystem::epoll::EPollEventType::EPOLLRDHUP
                                    | crate::filesystem::epoll::EPollEventType::EPOLLERR,
                            )
                        },
                        self.recv_timeout(),
                    );
                    if let Err(error) = wait_ret {
                        if total_read > 0 {
                            return Ok(total_read);
                        }
                        return Err(error);
                    }
                }
                Err(error) => {
                    if total_read > 0 {
                        return Ok(total_read);
                    }
                    return Err(error);
                }
            }
        }
    }

    pub(super) fn read_to_user_buffer_impl(
        &self,
        user_buffer: &mut UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        self.recv_to_user_buffer_impl(user_buffer, self.is_nonblock(), false)
    }

    pub(super) fn take_pending_error(&self) -> Option<SystemError> {
        match self.inner.read().as_ref() {
            Some(inner::Inner::Connecting(connecting)) => connecting.take_error(),
            Some(inner::Inner::Established(established)) => {
                established.with_mut(|socket| socket.take_reset().map(inner::reset_error))
            }
            _ => None,
        }
    }

    pub(super) fn try_send(&self, buf: &[u8], report_error: bool) -> Result<usize, SystemError> {
        // Background cork flushes use try_send_direct and must leave the error
        // available to SO_ERROR or a subsequent user I/O operation.
        if report_error {
            if let Some(error) = self.take_pending_error() {
                self.update_events();
                return Err(error);
            }
        }
        let result = self.try_send_inner(buf);
        if let Err(error) = result {
            let error = if report_error {
                self.take_pending_error().unwrap_or(error)
            } else {
                error
            };
            self.update_events();
            return Err(error);
        }
        result
    }

    fn try_send_inner(&self, buf: &[u8]) -> Result<usize, SystemError> {
        if buf.is_empty() {
            // Linux 语义：对 SOCK_STREAM，写入 0 字节应当立刻成功返回 0，且不阻塞。
            return Ok(0);
        }
        if self.is_send_shutdown() {
            return Err(SystemError::EPIPE);
        }

        // Cork enqueue uses the same cork -> inner -> SocketSet order as
        // shutdown. Checking the transport and accepting bytes are atomic with
        // respect to ingress, including a reset whose SO_ERROR was consumed.
        let cork_enabled = self
            .options
            .tcp_cork
            .load(core::sync::atomic::Ordering::Relaxed);
        let mut cork_buf = self.cork_buf.lock();
        if !cork_buf.is_empty() || cork_enabled {
            if self.is_send_shutdown() {
                return Err(SystemError::EPIPE);
            }
            let cap = self
                .send_buf_size()
                .load(core::sync::atomic::Ordering::Relaxed);
            let mut enqueue = |socket: &mut smoltcp::socket::tcp::Socket| {
                if !socket.may_send() {
                    return if matches!(
                        socket.state(),
                        smoltcp::socket::tcp::State::SynSent
                            | smoltcp::socket::tcp::State::SynReceived
                    ) {
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    } else {
                        Err(SystemError::EPIPE)
                    };
                }
                let free = cap.saturating_sub(cork_buf.len());
                if free == 0 {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                let n = core::cmp::min(free, buf.len());
                cork_buf.extend_from_slice(&buf[..n]);
                Ok(n)
            };
            let accepted = match self.inner.read().as_ref() {
                Some(inner::Inner::Established(established)) => established.with_mut(&mut enqueue),
                Some(inner::Inner::Connecting(connecting)) => connecting.with_mut(&mut enqueue),
                _ => Err(SystemError::EPIPE),
            }?;
            let flush = !cork_enabled
                || cork_buf.len()
                    >= self
                        .tcp_max_seg()
                        .load(core::sync::atomic::Ordering::Relaxed);
            drop(cork_buf);
            if flush {
                if let Err(error) = self.flush_cork_buffer() {
                    if error == SystemError::EAGAIN_OR_EWOULDBLOCK {
                        self.maybe_schedule_cork_timeout();
                    }
                    // Bytes were accepted before the flush. Preserve a later
                    // error and report this successful short write.
                }
            } else {
                self.maybe_schedule_cork_timeout();
            }
            return Ok(accepted);
        }
        drop(cork_buf);
        self.try_send_direct(buf)
    }

    pub(crate) fn flush_cork_buffer(&self) -> Result<(), SystemError> {
        if self
            .cork_flush_in_progress
            .compare_exchange(
                false,
                true,
                core::sync::atomic::Ordering::Acquire,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return Ok(());
        }
        let _guard = CorkFlushGuard(self);

        loop {
            let cork_buf = self.cork_buf.lock();
            if cork_buf.is_empty() {
                self.maybe_complete_shutdown_wr_fin();
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
        // TODO: add nonblock check of connecting socket
        //
        // IMPORTANT: to avoid "all sleepers, no pollers" stalls on loopback (gVisor BlockingLargeSend),
        // we must ensure the protocol stack is progressed:
        // - poll BEFORE sending: to drain acks/advance state and make more send capacity available
        // - poll AFTER sending: to actually transmit queued segments and process immediate loopback delivery
        // Additionally, wake the netns poll thread so timers/retransmits can progress even if callers sleep.
        let maybe_iface = self.stack_poll_snapshot();
        if let Some(iface) = maybe_iface.as_ref() {
            if let Some(netns) = iface.net_namespace() {
                netns.wakeup_tcp_poll();
            }
            // Loopback / fast-path correctness:
            // Poll once may only enqueue TX (or only process RX) without completing the
            // loopback roundtrip (TX->RX->ACK). If we return EAGAIN too early here,
            // acks processed shortly afterwards can free send buffer and make POLLOUT
            // appear spuriously (gVisor PollWithFullBufferBlocks).
            super::poll_util::poll_stack_batch(iface.as_ref());
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
                if let Some(netns) = iface.net_namespace() {
                    netns.wakeup_tcp_poll();
                }
                super::poll_util::poll_stack_batch(iface.as_ref());
            }
            return ret;
        }

        // Handle transition from Connecting to Established
        let mut writer = self.inner.write();
        if let Some(inner) = writer.take() {
            let ret = match inner {
                inner::Inner::Connecting(conn) => {
                    // A background flush may observe a failed connect, but
                    // must not consume its result or replace its owner state.
                    if !conn.is_connected() {
                        let error = if conn.failure_reason().is_some() || conn.is_refused_consumed()
                        {
                            SystemError::EPIPE
                        } else {
                            SystemError::EAGAIN_OR_EWOULDBLOCK
                        };
                        writer.replace(inner::Inner::Connecting(conn));
                        return Err(error);
                    }
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
                if let Some(netns) = iface.net_namespace() {
                    netns.wakeup_tcp_poll();
                }
                super::poll_util::poll_stack_batch(iface.as_ref());
            }
            return ret;
        }

        Err(SystemError::ENOTCONN)
    }
}

struct CorkFlushGuard<'a>(&'a TcpSocket);
impl Drop for CorkFlushGuard<'_> {
    fn drop(&mut self) {
        self.0
            .cork_flush_in_progress
            .store(false, core::sync::atomic::Ordering::Release);
    }
}

#[derive(Debug)]
struct CorkFlushTimer {
    socket: Weak<TcpSocket>,
}

impl TimerFunction for CorkFlushTimer {
    fn run(&mut self) -> Result<(), SystemError> {
        if let Some(socket) = self.socket.upgrade() {
            let socket = socket.clone();
            schedule_work(Work::new(move || {
                socket.handle_cork_timeout();
            }));
        }
        Ok(())
    }
}
