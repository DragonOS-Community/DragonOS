use system_error::SystemError::{self, *};
use core::sync::atomic::AtomicBool;

use crate::net::socket::common::Shutdown;
use crate::libs::rwlock::{RwLock, RwLockWriteGuard};
use smoltcp;

pub mod inner;
use inner::*;

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
                    (TcpStream { inner: stream }, remote)
                )
            }
            _ => Err(EINVAL),
        }
    }
}

struct TcpStream {
    inner: Established,
}

// impl TcpStream {
//     pub fn read(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
//         self.inner.recv_slice(buf)
//     }

//     pub fn write(&self, buf: &[u8]) -> Result<usize, SystemError> {
//         self.inner.send_slice(buf)
//     }
// }

