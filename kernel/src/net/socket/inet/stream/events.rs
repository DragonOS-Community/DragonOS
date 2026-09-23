use super::inner;
use super::TcpSocket;

type EP = crate::filesystem::epoll::EPollEventType;

impl TcpSocket {
    pub(crate) fn update_events(&self) -> bool {
        // If cork was disabled or SHUT_WR was requested while there are still cork-buffered bytes,
        // opportunistically flush them so the data does not become unreachable and FIN can be sent.
        if !self
            .cork_flush_in_progress
            .load(core::sync::atomic::Ordering::Relaxed)
            && (!self
                .options
                .tcp_cork
                .load(core::sync::atomic::Ordering::Relaxed)
                || self.is_send_shutdown())
            && !self.cork_buf.lock().is_empty()
        {
            let _ = self.flush_cork_buffer();
        }

        let mut inner_guard = self.inner.read();
        if let Some(inner::Inner::Connecting(connecting)) = inner_guard.as_ref() {
            let _ = connecting.update_io_events(&self.pollee);
            if connecting.is_connected() {
                // Completion belongs to the socket state machine, not to a
                // subsequent send(). Otherwise a peer's first data/FIN stays
                // hidden behind the Connecting event and receive paths.
                drop(inner_guard);
                {
                    let mut writer = self.inner.write();
                    if matches!(writer.as_ref(), Some(inner::Inner::Connecting(conn)) if conn.is_connected())
                    {
                        let Some(inner::Inner::Connecting(conn)) = writer.take() else {
                            unreachable!();
                        };
                        let (established, _) = conn.into_result();
                        writer.replace(established);
                    }
                }
                inner_guard = self.inner.read();
            }
        }
        if matches!(inner_guard.as_ref(), Some(inner::Inner::Listening(ls)) if ls.has_excess_slots())
        {
            // A pending handshake can return to LISTEN after RST without an
            // accept. Reclaim its surplus slot through the existing event path.
            // Never upgrade while retaining the read lock; close/listen may
            // change the state before the write lock is acquired.
            drop(inner_guard);
            {
                let mut writer = self.inner.write();
                if let Some(inner::Inner::Listening(listening)) = writer.as_mut() {
                    listening.trim_excess();
                }
            }
            inner_guard = self.inner.read();
        }
        match inner_guard.as_ref() {
            None => false,
            Some(inner::Inner::Init(_)) => {
                // Linux: POLLHUP is set on fresh socket.
                self.pollee.fetch_or(
                    EP::EPOLLHUP.bits() as usize,
                    core::sync::atomic::Ordering::Relaxed,
                );
                false
            }
            Some(inner::Inner::Closed(_)) => {
                // 显式关闭态：不再访问 smoltcp handle，只体现“已关闭”的可见事件。
                // 这里采用与 Init 一致的最小语义：设置 HUP，用于唤醒 poll/epoll 等等待者。
                self.pollee.fetch_or(
                    EP::EPOLLHUP.bits() as usize,
                    core::sync::atomic::Ordering::Relaxed,
                );
                false
            }
            Some(inner::Inner::Connecting(connecting)) => connecting.update_io_events(&self.pollee),
            Some(inner::Inner::Established(established)) => {
                established.update_io_events(&self.pollee, self.shutdown_bits());
                false
            }
            Some(inner::Inner::Listening(listening)) => {
                listening.update_io_events(&self.pollee);
                false
            }
        }
    }

    #[inline]
    pub fn incoming(&self) -> bool {
        let events = EP::from_bits_truncate(self.do_poll() as u32);
        events.contains(EP::EPOLLIN)
            || events.contains(EP::EPOLLHUP)
            || events.contains(EP::EPOLLERR)
    }

    #[inline]
    pub fn do_poll(&self) -> usize {
        self.pollee.load(core::sync::atomic::Ordering::SeqCst)
    }

    #[allow(dead_code)]
    pub fn can_recv(&self) -> bool {
        self.check_io_event().contains(EP::EPOLLIN)
    }
    #[allow(dead_code)]
    pub fn check_io_event(&self) -> crate::filesystem::epoll::EPollEventType {
        self.update_events();
        EP::from_bits_truncate(self.do_poll() as u32)
    }
}
