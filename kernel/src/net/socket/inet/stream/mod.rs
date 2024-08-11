use system_error::SystemError::{self, *};
use core::sync::atomic::AtomicBool;

use crate::net::socket::common::Shutdown;
use crate::libs::rwlock::{RwLock, RwLockWriteGuard};
use smoltcp;

pub mod inner;
use inner::*;

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
    fn write_state<F, R>(&self, mut f: F) -> R
    where 
        F: FnMut(RwLockWriteGuard<Option<Inner>>, Inner) -> R
    {
        let mut inner_guard = self.inner.write();
        let inner = inner_guard.take().expect("Tcp Inner is None");
        f(inner_guard, inner)
    }

    pub fn bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        self.write_state(|mut inner_guard, inner| {
            match inner {
                Inner::Unbound(unbound) => {
                    let connecting = unbound.bind(local_endpoint)?;
                    inner_guard.replace(Inner::Connecting(connecting));
                    Ok(())
                }
                _ => Err(EINVAL),
            }
        })
    }

    pub fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.write_state(|mut inner_guard, inner| {
            match inner {
                Inner::Connecting(connecting) => {
                    let listening = connecting.listen(backlog)?;
                    inner_guard.replace(Inner::Listening(listening));
                    Ok(())
                }
                _ => Err(EINVAL),
            }
        })
    }
}