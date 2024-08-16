
/// ### 为socket提供无锁的poll方法
///
/// 因为在网卡中断中，需要轮询socket的状态，如果使用socket文件或者其inode来poll
/// 在当前的设计，会必然死锁，所以引用这一个设计来解决，提供无🔓的poll
pub struct SocketPollMethod;

impl SocketPollMethod {
    pub fn poll(socket: &socket::Socket, handle_item: &SocketHandleItem) -> EPollEventType {
        let shutdown = handle_item.shutdown_type();
        match socket {
            socket::Socket::Udp(udp) => Self::udp_poll(udp, shutdown),
            socket::Socket::Tcp(tcp) => Self::tcp_poll(tcp, shutdown, handle_item.is_posix_listen),
            socket::Socket::Raw(raw) => Self::raw_poll(raw, shutdown),
            _ => todo!(),
        }
    }

    pub fn tcp_poll(
        socket: &tcp::Socket,
        shutdown: ShutdownType,
        is_posix_listen: bool,
    ) -> EPollEventType {
        let mut events = EPollEventType::empty();
        // debug!("enter tcp_poll! is_posix_listen:{}", is_posix_listen);
        // 处理listen的socket
        if is_posix_listen {
            // 如果是listen的socket，那么只有EPOLLIN和EPOLLRDNORM
            if socket.is_active() {
                events.insert(EPollEventType::EPOLL_LISTEN_CAN_ACCEPT);
            }

            // debug!("tcp_poll listen socket! events:{:?}", events);
            return events;
        }

        let state = socket.state();

        if shutdown == ShutdownType::SHUTDOWN_MASK || state == tcp::State::Closed {
            events.insert(EPollEventType::EPOLLHUP);
        }

        if shutdown.contains(ShutdownType::RCV_SHUTDOWN) {
            events.insert(
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM | EPollEventType::EPOLLRDHUP,
            );
        }

        // Connected or passive Fast Open socket?
        if state != tcp::State::SynSent && state != tcp::State::SynReceived {
            // socket有可读数据
            if socket.can_recv() {
                events.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
            }

            if !(shutdown.contains(ShutdownType::SEND_SHUTDOWN)) {
                // 缓冲区可写（这里判断可写的逻辑好像跟linux不太一样）
                if socket.send_queue() < socket.send_capacity() {
                    events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
                } else {
                    // TODO：触发缓冲区已满的信号SIGIO
                    todo!("A signal SIGIO that the buffer is full needs to be sent");
                }
            } else {
                // 如果我们的socket关闭了SEND_SHUTDOWN，epoll事件就是EPOLLOUT
                events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
            }
        } else if state == tcp::State::SynSent {
            events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
        }

        // socket发生错误
        // TODO: 这里的逻辑可能有问题，需要进一步验证是否is_active()==false就代表socket发生错误
        if !socket.is_active() {
            events.insert(EPollEventType::EPOLLERR);
        }

        events
    }

    pub fn udp_poll(socket: &udp::Socket, shutdown: ShutdownType) -> EPollEventType {
        let mut event = EPollEventType::empty();

        if shutdown.contains(ShutdownType::RCV_SHUTDOWN) {
            event.insert(
                EPollEventType::EPOLLRDHUP | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        if shutdown.contains(ShutdownType::SHUTDOWN_MASK) {
            event.insert(EPollEventType::EPOLLHUP);
        }

        if socket.can_recv() {
            event.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
        }

        if socket.can_send() {
            event.insert(
                EPollEventType::EPOLLOUT
                    | EPollEventType::EPOLLWRNORM
                    | EPollEventType::EPOLLWRBAND,
            );
        } else {
            // TODO: 缓冲区空间不够，需要使用信号处理
            todo!()
        }

        return event;
    }

    pub fn raw_poll(socket: &raw::Socket, shutdown: ShutdownType) -> EPollEventType {
        //debug!("enter raw_poll!");
        let mut event = EPollEventType::empty();

        if shutdown.contains(ShutdownType::RCV_SHUTDOWN) {
            event.insert(
                EPollEventType::EPOLLRDHUP | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        if shutdown.contains(ShutdownType::SHUTDOWN_MASK) {
            event.insert(EPollEventType::EPOLLHUP);
        }

        if socket.can_recv() {
            //debug!("poll can recv!");
            event.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
        } else {
            //debug!("poll can not recv!");
        }

        if socket.can_send() {
            //debug!("poll can send!");
            event.insert(
                EPollEventType::EPOLLOUT
                    | EPollEventType::EPOLLWRNORM
                    | EPollEventType::EPOLLWRBAND,
            );
        } else {
            //debug!("poll can not send!");
            // TODO: 缓冲区空间不够，需要使用信号处理
            todo!()
        }
        return event;
    }
}
