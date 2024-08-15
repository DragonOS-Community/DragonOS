use system_error::SystemError::{self, *};
use core::sync::atomic::AtomicBool;

use crate::net::event_poll::EPollEventType;
use crate::net::net_core::poll_ifaces;
use crate::net::socket::common::{PollUnit, Shutdown};
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
}

impl TcpSocket {
    pub fn new(nonblock: bool) -> Self {
        Self {
            inner: RwLock::new(Some(Inner::Unbound(Unbound::new()))),
            shutdown: Shutdown::new(),
            nonblock: AtomicBool::new(nonblock),
        }
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
        self.write_state(|inner| {
            match inner {
                Inner::Unbound(unbound) => {
                    unbound.bind(local_endpoint).map(|inner| 
                        Inner::Connecting(inner)
                    )
                }
                _ => Err(EINVAL),
            }
        })
    }

    pub fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.write_state(|inner| {
            match inner {
                Inner::Connecting(connecting) => {
                    connecting.listen(backlog).map(|inners| 
                        Inner::Listening(inners)
                    )
                }
                _ => Err(EINVAL),
            }
        })
    }

    pub fn accept(&self) -> Result<(TcpStream, smoltcp::wire::IpEndpoint), SystemError> {
        match self.inner.write().as_mut().expect("Tcp Inner is None") {
            Inner::Listening(listening) => {
                listening.accept().map(|(stream, remote)| 
                    ( 
                        TcpStream { 
                            inner: stream,
                            shutdown: Shutdown::new(),
                            nonblock: AtomicBool::new(
                                self.nonblock.load(
                                    core::sync::atomic::Ordering::Relaxed
                                )
                            ),
                            poll_unit: PollUnit::default(),
                        }, 
                        remote
                    )
                )
            }
            _ => Err(EINVAL),
        }
    }
}

#[derive(Debug)]
// #[cast_to([sync] IndexNode)]
struct TcpStream {
    inner: Established, 
    shutdown: Shutdown,
    nonblock: AtomicBool,
    poll_unit: PollUnit,
}

impl TcpStream {
    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if self.nonblock.load(core::sync::atomic::Ordering::Relaxed) {
            return self.recv_slice(buf);
        } else {
            return self.poll_unit().busy_wait(
                EP::EPOLLIN, 
                || self.recv_slice(buf)
            )
        }
    }

    pub fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        use smoltcp::socket::tcp::RecvError::*;
        let received = match self.inner.recv_slice(buf) {
            Ok(0) => Err(EAGAIN_OR_EWOULDBLOCK),
            Ok(size) => Ok(size),
            Err(InvalidState) => {
                log::error!("TcpStream::recv_slice: InvalidState");
                Err(EINVAL)
            },
            Err(Finished) => {
                // Remote send is shutdown
                self.shutdown.recv_shutdown();
                Err(ENOTCONN)
            }
        };
        poll_ifaces();
        received
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let sent = self.inner.send_slice(buf);
        poll_ifaces();
        sent
    }
}

use crate::net::socket::Socket;
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

    fn kernel_ioctl(
            &self,
            arg: alloc::sync::Arc<dyn crate::net::event_poll::KernelIoctlData>,
            data: &crate::filesystem::vfs::FilePrivateData,
        ) -> Result<usize, SystemError> {
        drop(data);
        let epitem = arg
            .arc_any()
            .downcast::<crate::net::event_poll::EPollItem>()
            .map_err(|_| SystemError::EFAULT)?;

        // let _ = UserBufferReader::new(
        //     &epitem as *const Arc<EPollItem>,
        //     core::mem::size_of::<Arc<EPollItem>>(),
        //     false,
        // )?;

        // let core = tty.core();

        // core.add_epitem(epitem.clone());

        return Ok(0);
    }

    fn poll(&self, private_data: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, SystemError> {
        drop(private_data);
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
            return Ok(mask.bits() as usize);
        })
    }
}

impl Socket for TcpStream {
    fn poll_unit(&self) -> &PollUnit {
        &self.poll_unit
    }
}