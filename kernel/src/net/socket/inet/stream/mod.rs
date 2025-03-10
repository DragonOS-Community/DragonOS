use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize};
use system_error::SystemError::{self, *};

use crate::libs::rwlock::RwLock;
use crate::net::event_poll::EPollEventType;
use crate::net::socket::*;
use crate::sched::SchedMode;
use inet::{InetSocket, UNSPECIFIED_LOCAL_ENDPOINT_V4, UNSPECIFIED_LOCAL_ENDPOINT_V6};
use smoltcp;

mod inner;
use inner::*;

mod option;
pub use option::Options as TcpOption;

type EP = EPollEventType;
#[derive(Debug)]
pub struct TcpSocket {
    inner: RwLock<Option<Inner>>,
    #[allow(dead_code)]
    shutdown: Shutdown, // TODO set shutdown status
    nonblock: AtomicBool,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
    pollee: AtomicUsize,
}

impl TcpSocket {
    pub fn new(_nonblock: bool, ver: smoltcp::wire::IpVersion) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(Inner::Init(Init::new(ver)))),
            shutdown: Shutdown::new(),
            nonblock: AtomicBool::new(false),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
            pollee: AtomicUsize::new(0_usize),
        })
    }

    pub fn new_established(inner: Established, nonblock: bool) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(Inner::Established(inner))),
            shutdown: Shutdown::new(),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
            pollee: AtomicUsize::new((EP::EPOLLIN.bits() | EP::EPOLLOUT.bits()) as usize),
        })
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        match writer.take().expect("Tcp Inner is None") {
            Inner::Init(inner) => {
                let bound = inner.bind(local_endpoint)?;
                if let Init::Bound((ref bound, _)) = bound {
                    bound
                        .iface()
                        .common()
                        .bind_socket(self.self_ref.upgrade().unwrap());
                }
                writer.replace(Inner::Init(bound));
                Ok(())
            }
            any => {
                writer.replace(any);
                log::error!("TcpSocket::do_bind: not Init");
                Err(EINVAL)
            }
        }
    }

    pub fn do_listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp Inner is None");
        let (listening, err) = match inner {
            Inner::Init(init) => {
                let listen_result = init.listen(backlog);
                match listen_result {
                    Ok(listening) => (Inner::Listening(listening), None),
                    Err((init, err)) => (Inner::Init(init), Some(err)),
                }
            }
            _ => (inner, Some(EINVAL)),
        };
        writer.replace(listening);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn try_accept(&self) -> Result<(Arc<TcpSocket>, smoltcp::wire::IpEndpoint), SystemError> {
        match self.inner.write().as_mut().expect("Tcp Inner is None") {
            Inner::Listening(listening) => listening.accept().map(|(stream, remote)| {
                (
                    TcpSocket::new_established(stream, self.is_nonblock()),
                    remote,
                )
            }),
            _ => Err(EINVAL),
        }
    }

    // SHOULD refactor
    pub fn start_connect(
        &self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
    ) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp Inner is None");
        let (init, result) = match inner {
            Inner::Init(init) => {
                let conn_result = init.connect(remote_endpoint);
                match conn_result {
                    Ok(connecting) => (
                        Inner::Connecting(connecting),
                        if !self.is_nonblock() {
                            Ok(())
                        } else {
                            Err(EINPROGRESS)
                        },
                    ),
                    Err((init, err)) => (Inner::Init(init), Err(err)),
                }
            }
            Inner::Connecting(connecting) if self.is_nonblock() => {
                (Inner::Connecting(connecting), Err(EALREADY))
            }
            Inner::Connecting(connecting) => (Inner::Connecting(connecting), Ok(())),
            Inner::Listening(inner) => (Inner::Listening(inner), Err(EISCONN)),
            Inner::Established(inner) => (Inner::Established(inner), Err(EISCONN)),
        };

        match result {
            Ok(()) | Err(EINPROGRESS) => {
                init.iface().unwrap().poll();
            }
            _ => {}
        }

        writer.replace(init);
        return result;
    }

    // for irq use
    pub fn finish_connect(&self) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let Inner::Connecting(conn) = writer.take().expect("Tcp Inner is None") else {
            log::error!("TcpSocket::finish_connect: not Connecting");
            return Err(EINVAL);
        };

        let (inner, result) = conn.into_result();
        writer.replace(inner);
        drop(writer);

        result
    }

    pub fn check_connect(&self) -> Result<(), SystemError> {
        self.update_events();
        let mut write_state = self.inner.write();
        let inner = write_state.take().expect("Tcp Inner is None");
        let (replace, result) = match inner {
            Inner::Connecting(conn) => conn.into_result(),
            Inner::Established(es) => {
                log::warn!("TODO: check new established");
                (Inner::Established(es), Ok(()))
            } // TODO check established
            _ => {
                log::warn!("TODO: connecting socket error options");
                (inner, Err(EINVAL))
            } // TODO socket error options
        };
        write_state.replace(replace);
        result
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.inner
            .read()
            .as_ref()
            .map(|inner| {
                inner.iface().unwrap().poll();
                let result = match inner {
                    Inner::Established(inner) => inner.recv_slice(buf),
                    _ => Err(EINVAL),
                };
                inner.iface().unwrap().poll();
                result
            })
            .unwrap()
    }

    pub fn try_send(&self, buf: &[u8]) -> Result<usize, SystemError> {
        // TODO: add nonblock check of connecting socket
        let sent = match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Established(inner) => inner.send_slice(buf),
            _ => Err(EINVAL),
        };
        self.inner.read().as_ref().unwrap().iface().unwrap().poll();
        sent
    }

    fn update_events(&self) -> bool {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Init(_) => false,
            Inner::Connecting(connecting) => connecting.update_io_events(),
            Inner::Established(established) => {
                established.update_io_events(&self.pollee);
                false
            }
            Inner::Listening(listening) => {
                listening.update_io_events(&self.pollee);
                false
            }
        }
    }

    fn incoming(&self) -> bool {
        EP::from_bits_truncate(self.poll() as u32).contains(EP::EPOLLIN)
    }
}

impl Socket for TcpSocket {
    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn get_name(&self) -> Result<Endpoint, SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Init(Init::Unbound((_, ver))) => Ok(Endpoint::Ip(match ver {
                smoltcp::wire::IpVersion::Ipv4 => UNSPECIFIED_LOCAL_ENDPOINT_V4,
                smoltcp::wire::IpVersion::Ipv6 => UNSPECIFIED_LOCAL_ENDPOINT_V6,
            })),
            Inner::Init(Init::Bound((_, local))) => Ok(Endpoint::Ip(*local)),
            Inner::Connecting(connecting) => Ok(Endpoint::Ip(connecting.get_name())),
            Inner::Established(established) => Ok(Endpoint::Ip(established.get_name())),
            Inner::Listening(listening) => Ok(Endpoint::Ip(listening.get_name())),
        }
    }

    fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Init(_) => Err(ENOTCONN),
            Inner::Connecting(connecting) => Ok(Endpoint::Ip(connecting.get_peer_name())),
            Inner::Established(established) => Ok(Endpoint::Ip(established.get_peer_name())),
            Inner::Listening(_) => Err(ENOTCONN),
        }
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(addr) = endpoint {
            return self.do_bind(addr);
        }
        log::debug!("TcpSocket::bind: invalid endpoint");
        return Err(EINVAL);
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let Endpoint::Ip(endpoint) = endpoint else {
            log::debug!("TcpSocket::connect: invalid endpoint");
            return Err(EINVAL);
        };
        self.start_connect(endpoint)?; // Only Nonblock or error will return error.

        return loop {
            match self.check_connect() {
                Err(EAGAIN_OR_EWOULDBLOCK) => {}
                result => break result,
            }
        };
    }

    fn poll(&self) -> usize {
        self.pollee.load(core::sync::atomic::Ordering::SeqCst)
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.do_listen(backlog)
    }

    fn accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        if self.is_nonblock() {
            self.try_accept()
        } else {
            loop {
                match self.try_accept() {
                    Err(EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.incoming(), {})?;
                    }
                    result => break result,
                }
            }
        }
        .map(|(inner, endpoint)| (Inode::new(inner), Endpoint::Ip(endpoint)))
    }

    fn recv(&self, buffer: &mut [u8], _flags: PMSG) -> Result<usize, SystemError> {
        self.try_recv(buffer)
    }

    fn send(&self, buffer: &[u8], _flags: PMSG) -> Result<usize, SystemError> {
        self.try_send(buffer)
    }

    fn send_buffer_size(&self) -> usize {
        self.inner
            .read()
            .as_ref()
            .expect("Tcp Inner is None")
            .send_buffer_size()
    }

    fn recv_buffer_size(&self) -> usize {
        self.inner
            .read()
            .as_ref()
            .expect("Tcp Inner is None")
            .recv_buffer_size()
    }

    fn shutdown(&self, how: ShutdownTemp) -> Result<(), SystemError> {
        let self_shutdown = self.shutdown.get().bits();
        let diff = how.bits().difference(self_shutdown);
        match diff.is_empty() {
            true => return Ok(()),
            false => {
                if diff.contains(ShutdownBit::SHUT_RD) {
                    self.shutdown.recv_shutdown();
                    // TODO 协议栈处理
                }
                if diff.contains(ShutdownBit::SHUT_WR) {
                    self.shutdown.send_shutdown();
                    // TODO 协议栈处理
                }
            }
        }
        Ok(())
    }

    fn close(&self) -> Result<(), SystemError> {
        let Some(inner) = self.inner.write().take() else {
            log::warn!("TcpSocket::close: already closed, unexpected");
            return Ok(());
        };
        if let Some(iface) = inner.iface() {
            iface
                .common()
                .unbind_socket(self.self_ref.upgrade().unwrap());
        }

        match inner {
            // complete connecting socket close logic
            Inner::Connecting(conn) => {
                let conn = unsafe { conn.into_established() };
                conn.close();
                conn.release();
            }
            Inner::Established(es) => {
                es.close();
                es.release();
            }
            Inner::Listening(ls) => {
                ls.close();
                ls.release();
            }
            Inner::Init(init) => {
                init.close();
            }
        };

        Ok(())
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        if level != PSOL::TCP {
            // return Err(EINVAL);
            log::debug!("TcpSocket::set_option: not TCP");
            return Ok(());
        }
        use option::Options::{self, *};
        let option_name = Options::try_from(name as i32)?;
        log::debug!("TCP Option: {:?}, value = {:?}", option_name, val);
        match option_name {
            NoDelay => {
                let nagle_enabled = val[0] != 0;
                let mut writer = self.inner.write();
                let inner = writer.take().expect("Tcp Inner is None");
                match inner {
                    Inner::Established(established) => {
                        established.with_mut(|socket| {
                            socket.set_nagle_enabled(nagle_enabled);
                        });
                        writer.replace(Inner::Established(established));
                    }
                    _ => {
                        writer.replace(inner);
                        return Err(EINVAL);
                    }
                }
            }
            KeepIntvl => {
                if val.len() == 4 {
                    let mut writer = self.inner.write();
                    let inner = writer.take().expect("Tcp Inner is None");
                    match inner {
                        Inner::Established(established) => {
                            let interval = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                            established.with_mut(|socket| {
                                socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(
                                    interval as u64,
                                )));
                            });
                            writer.replace(Inner::Established(established));
                        }
                        _ => {
                            writer.replace(inner);
                            return Err(EINVAL);
                        }
                    }
                } else {
                    return Err(EINVAL);
                }
            }
            KeepCnt => {
                // if val.len() == 4 {
                //     let mut writer = self.inner.write();
                //     let inner = writer.take().expect("Tcp Inner is None");
                //     match inner {
                //         Inner::Established(established) => {
                //             let count = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                //             established.with_mut(|socket| {
                //                 socket.set_keep_alive_count(count);
                //             });
                //             writer.replace(Inner::Established(established));
                //         }
                //         _ => {
                //             writer.replace(inner);
                //             return Err(EINVAL);
                //         }
                //     }
                // } else {
                //     return Err(EINVAL);
                // }
            }
            KeepIdle => {}
            _ => {
                log::debug!("TcpSocket::set_option: not supported");
                // return Err(ENOPROTOOPT);
            }
        }
        Ok(())
    }
}

impl InetSocket for TcpSocket {
    fn on_iface_events(&self) {
        if self.update_events() {
            let _result = self.finish_connect();
            // set error
        }
    }
}
