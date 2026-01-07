use super::inner;
use super::TcpSocket;

type EP = crate::filesystem::epoll::EPollEventType;

impl TcpSocket {
    pub(crate) fn update_events(&self) -> bool {
        let inner_guard = self.inner.read();
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
                established.update_io_events(&self.pollee);

                // If SHUT_WR, set EPOLLOUT so send() wakes up and returns EPIPE.
                if self.is_send_shutdown() {
                    self.pollee.fetch_or(
                        (EP::EPOLLOUT | EP::EPOLLWRNORM).bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                }
                // If SHUT_RD, set EPOLLIN so recv() wakes up and returns 0 (EOF).
                if self.is_recv_shutdown() {
                    self.pollee.fetch_or(
                        (EP::EPOLLIN | EP::EPOLLRDNORM).bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                }

                // Note: EPOLLHUP/EPOLLRDHUP/EPOLLERR are now handled in
                // Established::update_io_events() based on socket state.
                false
            }
            Some(inner::Inner::SelfConnected(sc)) => {
                // Self-connect is modeled by an internal receive queue. Readable becomes true
                // when the queue has data OR after SHUT_WR (EOF). Writable depends on queue
                // free space unless SHUT_WR (then send() returns EPIPE).
                sc.update_io_events(&self.pollee, self.is_send_shutdown());

                // Match established behavior for shutdown bits.
                if self.is_send_shutdown() {
                    self.pollee.fetch_or(
                        (EP::EPOLLOUT | EP::EPOLLWRNORM).bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                }
                if self.is_recv_shutdown() {
                    self.pollee.fetch_or(
                        (EP::EPOLLIN | EP::EPOLLRDNORM).bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                }
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
        EP::from_bits_truncate(self.do_poll() as u32).contains(EP::EPOLLIN)
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
