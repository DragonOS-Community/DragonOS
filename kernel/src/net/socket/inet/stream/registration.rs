//! Linearizes TCP connection-state publication with iface notification ownership.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::libs::mutex::Mutex;
use crate::net::{socket, Iface};

#[derive(Debug)]
pub(super) struct ConnectingRegistration {
    state: Arc<ConnectingRegistrationState>,
    retained: bool,
}

#[derive(Debug)]
struct ConnectingRegistrationState {
    iface: Arc<dyn Iface>,
    wrapper: Weak<dyn socket::inet::InetSocket>,
    state: AtomicUsize,
    routing_publication: Mutex<Option<socket::inet::common::RoutedSocketPublication>>,
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

    pub(super) fn new(iface: Arc<dyn Iface>, wrapper: Weak<dyn socket::inet::InetSocket>) -> Self {
        let routing_publication =
            socket::inet::common::RoutedSocketPublication::begin(iface.clone());
        Self {
            state: Arc::new(ConnectingRegistrationState {
                iface,
                wrapper,
                state: AtomicUsize::new(Self::PENDING),
                routing_publication: Mutex::new(Some(routing_publication)),
            }),
            retained: false,
        }
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
    fn release_routing_publication(&self) {
        drop(self.routing_publication.lock().take());
    }

    fn cancel(&self) {
        let previous = self
            .state
            .swap(ConnectingRegistration::CANCELLED, Ordering::AcqRel);
        if matches!(
            previous,
            ConnectingRegistration::PUBLISHED | ConnectingRegistration::RETAINED
        ) {
            if let Some(wrapper) = self.wrapper.upgrade() {
                self.iface.common().unbind_socket(wrapper);
            }
            self.release_routing_publication();
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
            self.0.release_routing_publication();
            return;
        };

        // bind_socket is idempotent under one bounds lock, so an explicitly
        // bound socket never observes an unregister/register gap here.
        self.0.iface.common().bind_socket(wrapper.clone());
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
                self.0.iface.common().unbind_socket(wrapper);
            }
            Err(_) => unreachable!(),
        }

        // Either bounds owns the notification lifetime, or cancellation has
        // rolled the insertion back. This is the only safe handoff point.
        self.0.release_routing_publication();
    }
}

impl ConnectingRegistrationLease {
    pub(super) fn cancel(&self) {
        self.0.cancel();
    }
}
