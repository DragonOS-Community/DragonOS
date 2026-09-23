//! Linearizes TCP connection-state publication with stack notification ownership.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::net::socket;
use system_error::SystemError;

#[derive(Debug)]
pub(super) struct ConnectingRegistration {
    state: Arc<ConnectingRegistrationState>,
    retained: bool,
}

#[derive(Debug)]
struct ConnectingRegistrationState {
    stack: Arc<crate::net::tcp_stack::TcpStack>,
    wrapper: Weak<dyn socket::inet::InetSocket>,
    state: AtomicUsize,
}

#[derive(Debug)]
pub(super) struct ConnectingRegistrationPublisher(Arc<ConnectingRegistrationState>);

#[derive(Debug)]
pub(super) struct ConnectingRegistrationLease(Arc<ConnectingRegistrationState>);

impl ConnectingRegistration {
    const PENDING: usize = 0;
    const PUBLISHED: usize = 1;
    const RETAINED: usize = 2;
    const CANCELLED: usize = 3;

    pub(super) fn try_new(
        stack: Arc<crate::net::tcp_stack::TcpStack>,
        wrapper: Weak<dyn socket::inet::InetSocket>,
    ) -> Result<Self, SystemError> {
        Ok(Self {
            state: Arc::try_new(ConnectingRegistrationState {
                stack,
                wrapper,
                state: AtomicUsize::new(Self::PENDING),
            })
            .map_err(|_| SystemError::ENOMEM)?,
            retained: false,
        })
    }

    pub(super) fn publisher(&self) -> ConnectingRegistrationPublisher {
        ConnectingRegistrationPublisher(self.state.clone())
    }

    pub(super) fn retain(&mut self) -> ConnectingRegistrationLease {
        self.state.state.store(Self::RETAINED, Ordering::Release);
        self.retained = true;
        ConnectingRegistrationLease(self.state.clone())
    }

    pub(super) fn cancel(&self) {
        self.state.cancel();
    }
}

impl ConnectingRegistrationState {
    fn cancel(&self) {
        let previous = self
            .state
            .swap(ConnectingRegistration::CANCELLED, Ordering::AcqRel);
        if matches!(
            previous,
            ConnectingRegistration::PUBLISHED | ConnectingRegistration::RETAINED
        ) {
            if let Some(wrapper) = self.wrapper.upgrade() {
                self.stack.unbind_socket(wrapper);
            }
        }
    }
}

impl Drop for ConnectingRegistration {
    fn drop(&mut self) {
        if !self.retained {
            self.state.cancel();
        }
    }
}

impl ConnectingRegistrationPublisher {
    /// Publishes after `TcpSocket::inner` has been released, then validates
    /// that the connection attempt was not concurrently consumed or closed.
    pub(super) fn publish(self) {
        let Some(wrapper) = self.0.wrapper.upgrade() else {
            self.0
                .state
                .store(ConnectingRegistration::CANCELLED, Ordering::Release);
            return;
        };

        // bind_socket is idempotent under one bounds lock, so an explicitly
        // bound socket never observes an unregister/register gap here.
        self.0.stack.bind_socket(wrapper.clone());
        match self.0.state.compare_exchange(
            ConnectingRegistration::PENDING,
            ConnectingRegistration::PUBLISHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_)
            | Err(ConnectingRegistration::PUBLISHED)
            | Err(ConnectingRegistration::RETAINED) => {}
            Err(ConnectingRegistration::CANCELLED) => {
                self.0.stack.unbind_socket(wrapper);
            }
            Err(_) => unreachable!(),
        }
    }
}

impl ConnectingRegistrationLease {
    pub(super) fn cancel(&self) {
        self.0.cancel();
    }
}
