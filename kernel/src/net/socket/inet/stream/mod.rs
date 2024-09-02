use system_error::SystemError::{self, *};
use core::sync::atomic::AtomicBool;
use alloc::sync::{Arc, Weak};

use crate::net::event_poll::EPollEventType;
use crate::net::net_core::poll_ifaces;
use crate::net::socket::common::{poll_unit::{EPollItems, WaitQueue}, Shutdown};
use crate::libs::rwlock::RwLock;
use smoltcp;

pub mod inner;
use inner::*;

type EP = EPollEventType;
#[derive(Debug)]
pub struct TcpSocket {
    inner: RwLock<Option<Inner>>,
    shutdown: Shutdown,
    nonblock: AtomicBool,
    epitems: EPollItems,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
}

impl TcpSocket {
    pub fn new(nonblock: bool) -> Arc<Self> {
        Arc::new_cyclic(
            |me| Self {
                inner: RwLock::new(Some(Inner::Init(Init::new()))),
                shutdown: Shutdown::new(),
                nonblock: AtomicBool::new(nonblock),
                epitems: EPollItems::default(),
                wait_queue: WaitQueue::default(),
                self_ref: me.clone(),
            }
        )
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    #[inline]
    fn write_state<F>(&self, mut f: F) -> Result<(), SystemError>
    where 
        F: FnMut(Inner) -> Result<Inner, SystemError>
    {
        let mut inner_guard = self.inner.write();
        let inner = inner_guard.take().expect("Tcp Inner is None");
        let update = f(inner)?;
        inner_guard.replace(update);
        Ok(())
    }

    pub fn bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        match writer.take().expect("Tcp Inner is None") {
            Inner::Init(inner) => {
                let bound = inner.bind(local_endpoint)?;
                if let Init::Bound((ref bound, _)) = bound {
                    bound.iface().common().bind_socket(self.self_ref.upgrade().unwrap());
                }
                writer.replace(Inner::Init(bound));
                Ok(())
            }
            _ => Err(EINVAL),
        }
    }

    pub fn do_listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp Inner is None");
        let (listening, err) = match inner {
            Inner::Init(init) => {
                let listen_result = init.listen(backlog);
                match listen_result {
                    Ok(listening) => {
                        (Inner::Listening(listening), None)
                    }
                    Err((init, err)) => {
                        (Inner::Init(init), Some(err))
                    }
                }
            }
            _ => {
                (inner, Some(EINVAL))
            }
        };
        writer.replace(listening);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn try_accept(&self) -> Result<(Arc<TcpStream>, smoltcp::wire::IpEndpoint), SystemError> {
        match self.inner.write().as_mut().expect("Tcp Inner is None") {
            Inner::Listening(listening) => {
                listening.accept().map(|(stream, remote)| 
                    ( 
                        Arc::new_cyclic( |me| TcpStream { 
                            inner: stream,
                            shutdown: Shutdown::new(),
                            nonblock: AtomicBool::new(
                                self.nonblock.load(
                                    core::sync::atomic::Ordering::Relaxed
                                )
                            ),
                            epitems: EPollItems::new(),
                            wait_queue: WaitQueue::default(),
                            self_ref: me.clone(),
                        }),
                        remote
                    )
                )
            }
            _ => Err(EINVAL),
        }
    }

    pub fn start_connect(&self, remote_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp Inner is None");
        let (init, err) = match inner {
            Inner::Init(init) => {
                let conn_result = init.connect(remote_endpoint);
                match conn_result {
                    Ok(connecting) => {
                        (
                            Inner::Connecting(connecting), 
                            if self.is_nonblock() {
                                None
                            } else {
                                Some(EINPROGRESS)
                            }
                        )
                    }
                    Err((init, err)) => {
                        (Inner::Init(init), Some(err))
                    }
                }
            }
            Inner::Connecting(connecting) if self.is_nonblock() => {
                (Inner::Connecting(connecting), Some(EALREADY))
            },
            Inner::Connecting(connecting) => {
                (Inner::Connecting(connecting), None)
            }
            Inner::Listening(inner) => {
                (Inner::Listening(inner), Some(EISCONN))
            }
            Inner::Established(inner) => {
                (Inner::Established(inner), Some(EISCONN))
            }
        };
        writer.replace(init);

        drop(writer);

        poll_ifaces();

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn finish_connect(&self) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let Inner::Connecting(conn) = writer.take().expect("Tcp Inner is None") else {
            log::error!("TcpSocket::finish_connect: not Connecting");
            return Err(EINVAL);
        };

        let (inner, err) = conn.into_result();
        writer.replace(inner);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn check_connect(&self) -> Result<(), SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Connecting(_) => Err(EAGAIN_OR_EWOULDBLOCK),
            Inner::Established(_) => Ok(()), // TODO check established
            _ => Err(EINVAL), // TODO socket error options
        }
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Established(inner) => {
                inner.recv_slice(buf)
            }
            _ => Err(EINVAL),
        }
    }

    pub fn try_send(&self, buf: &[u8]) -> Result<usize, SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Established(inner) => {
                let sent = inner.send_slice(buf);
                poll_ifaces();
                sent
            }
            _ => Err(EINVAL),
        }
    }
}

impl IndexNode for TcpSocket {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        // self.inner.read().as_ref().expect("Tcp Inner is None").read(buf)
        todo!()
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        // self.inner.read().as_ref().expect("Tcp Inner is None").write(buf)
        todo!()
    }

    fn fs(&self) -> alloc::sync::Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!("TcpSocket::fs")
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        todo!("TcpSocket::list")
    }

    fn kernel_ioctl(
            &self,
            arg: alloc::sync::Arc<dyn crate::net::event_poll::KernelIoctlData>,
            _data: &crate::filesystem::vfs::FilePrivateData,
        ) -> Result<usize, SystemError> {
        let epitem = arg
            .arc_any()
            .downcast::<crate::net::event_poll::EPollItem>()
            .map_err(|_| SystemError::EFAULT)?;

        self.epoll_items().add(epitem);

        return Ok(0);
    }

    fn poll(&self, private_data: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, SystemError> {
        drop(private_data);
        self.update_io_events().map(|x| x.bits() as usize)   
    }
}

impl Socket for TcpSocket {
    fn epoll_items(&self) -> EPollItems {
        self.epitems.clone()
    }

    fn wait_queue(&self) -> WaitQueue {
        self.wait_queue.clone()
    }

    fn update_io_events(&self) -> Result<EPollEventType, SystemError> {
        let mut mask = EP::empty();
        let shutdown = self.shutdown.get();
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Established(inner) => {
                inner.with_mut(|socket| {
                    let state = socket.state();
                    use smoltcp::socket::tcp::State::*;
                    type EP = crate::net::event_poll::EPollEventType;
                    
                    if shutdown.is_both_shutdown() ||
                        state == Closed ||
                        state == CloseWait
                    {
                        mask |= EP::EPOLLHUP;
                    }

                    if shutdown.is_recv_shutdown() {
                        mask |= EP::EPOLLIN | EP::EPOLLRDNORM | EP::EPOLLRDHUP;
                    }

                    if state != SynSent && state != SynReceived {
                        if socket.can_recv() {
                            mask |= EP::EPOLLIN | EP::EPOLLRDNORM;
                        }

                        if !shutdown.is_send_shutdown() {
                            if socket.can_send() {
                                mask |= EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND;
                            } else {
                                todo!("TcpSocket::poll: buffer space not enough");
                            }
                        } else {
                            mask |= EP::EPOLLOUT | EP::EPOLLWRNORM;
                        }
                        // TODO tcp urg data => EPOLLPRI
                    } else if state == SynSent /* inet_test_bit */ {
                        log::warn!("Active TCP fastopen socket with defer_connect");
                        mask |= EP::EPOLLOUT | EP::EPOLLWRNORM;
                    }

                    // TODO socket error
                    return Ok(mask);
                })
            }
            _ => Err(EINVAL),
        }
    }

    fn accept(&self) -> Result<(Arc<Inode>, crate::net::socket::Endpoint), SystemError> {
        self.try_accept().map(|(stream, remote)| 
            (stream as Arc<Inode>, crate::net::socket::Endpoint::from(remote))
        )
    }
}

#[derive(Debug)]
// #[cast_to([sync] IndexNode)]
struct TcpStream {
    inner: Established, 
    shutdown: Shutdown,
    nonblock: AtomicBool,
    epitems: EPollItems,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
}

impl TcpStream {
    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if self.nonblock.load(core::sync::atomic::Ordering::Relaxed) {
            return self.recv_slice(buf);
        } else {
            return self.wait_queue().busy_wait(
                EP::EPOLLIN, 
                || self.recv_slice(buf)
            )
        }
    }

    pub fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let received = self.inner.recv_slice(buf);
        poll_ifaces();
        received
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let sent = self.inner.send_slice(buf);
        poll_ifaces();
        sent
    }
}

use crate::net::socket::{Inode, Socket};
use crate::filesystem::vfs::IndexNode;

impl IndexNode for TcpStream {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        self.read(buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        self.send_slice(buf)
    }

    fn fs(&self) -> alloc::sync::Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!("TcpSocket::fs")
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        todo!("TcpSocket::list")
    }

    // fn kernel_ioctl(
    //         &self,
    //         arg: alloc::sync::Arc<dyn crate::net::event_poll::KernelIoctlData>,
    //         _data: &crate::filesystem::vfs::FilePrivateData,
    //     ) -> Result<usize, SystemError> {
    //     let epitem = arg
    //         .arc_any()
    //         .downcast::<crate::net::event_poll::EPollItem>()
    //         .map_err(|_| SystemError::EFAULT)?;

    //     self.epoll_items().add(epitem);

    //     return Ok(0);
    // }

}

impl Socket for TcpStream {
    fn epoll_items(&self) -> EPollItems {
        self.epitems.clone()
    }

    fn wait_queue(&self) -> WaitQueue {
        self.wait_queue.clone()
    }

    fn update_io_events(&self) -> Result<EPollEventType, SystemError> {
        self.inner.with(|socket| {
            let mut mask = EPollEventType::empty();
            let shutdown = self.shutdown.get();
            let state = socket.state();
            use smoltcp::socket::tcp::State::*;
            type EP = crate::net::event_poll::EPollEventType;
            
            if shutdown.is_both_shutdown() || state == Closed {
                mask |= EP::EPOLLHUP;
            }

            if shutdown.is_recv_shutdown() {
                mask |= EP::EPOLLIN | EP::EPOLLRDNORM | EP::EPOLLRDHUP;
            }

            if state != SynSent && state != SynReceived {
                if socket.can_recv() {
                    mask |= EP::EPOLLIN | EP::EPOLLRDNORM;
                }

                if !shutdown.is_send_shutdown() {
                    // __sk_stream_is_writeable，这是一个内联函数，用于判断一个TCP套接字是否可写。
                    // 
                    // 以下是函数的逐行解释：
                    // static inline bool __sk_stream_is_writeable(const struct sock *sk, int wake)
                    // - 这行定义了函数__sk_stream_is_writeable，它是一个内联函数（static inline），
                    // 这意味着在调用点直接展开代码，而不是调用函数体。函数接收两个参数：
                    // 一个指向struct sock对象的指针sk（代表套接字），和一个整型变量wake。
                    // 
                    // return sk_stream_wspace(sk) >= sk_stream_min_wspace(sk) &&
                    // - 这行代码调用了sk_stream_wspace函数，获取套接字sk的可写空间（write space）大小。
                    // 随后与sk_stream_min_wspace调用结果进行比较，该函数返回套接字为了保持稳定写入速度所需的
                    // 最小可写空间。如果当前可写空间大于或等于最小可写空间，则表达式为真。
                    //       __sk_stream_memory_free(sk, wake);
                    // - 这行代码调用了__sk_stream_memory_free函数，它可能用于检查套接字的内存缓冲区是否
                    // 有足够的空间可供写入数据。参数wake可能用于通知网络协议栈有数据需要发送，如果设置了相应的标志。
                    // 综上所述，__sk_stream_is_writeable函数的目的是判断一个TCP套接字是否可以安全地进行写操作，
                    // 它基于套接字的当前可写空间和所需的最小空间以及内存缓冲区的可用性。只有当这两个条件都满足时，
                    // 函数才会返回true，表示套接字是可写的。
                    if socket.can_send() {
                        mask |= EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND;
                    } else {
                        todo!("TcpStream::poll: buffer space not enough");
                    }
                } else {
                    mask |= EP::EPOLLOUT | EP::EPOLLWRNORM;
                }
                // TODO tcp urg data => EPOLLPRI
            } else if state == SynSent /* inet_test_bit */ {
                log::warn!("Active TCP fastopen socket with defer_connect");
                mask |= EP::EPOLLOUT | EP::EPOLLWRNORM;
            }

            // TODO socket error
            return Ok(mask);
        })
    }
}