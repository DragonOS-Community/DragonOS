//! Pre-born notification ownership, linearized with connection publication.
use crate::{
    libs::mutex::Mutex,
    net::{
        socket::inet::InetSocket,
        tcp_bounds::{BoundSlotReservation, PublishedBoundSlot},
    },
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

#[derive(Debug)]
pub(super) struct ConnectingRegistration {
    state: Arc<RegistrationState>,
    retained: bool,
}
#[derive(Debug)]
struct Publication {
    reserved: Option<BoundSlotReservation>,
    published: Option<PublishedBoundSlot>,
    cancelled: bool,
}
#[derive(Debug)]
struct RegistrationState {
    wrapper: Weak<dyn InetSocket>,
    publication: Mutex<Publication>,
}
#[derive(Debug)]
pub(super) struct ConnectingRegistrationPublisher(Arc<RegistrationState>);
#[derive(Debug)]
pub(super) struct ConnectingRegistrationLease(Arc<RegistrationState>);

impl ConnectingRegistration {
    /// Task preparation before inner/SocketSet; no vacancy growth after SYN.
    pub(super) fn try_new(
        reserved: BoundSlotReservation,
        wrapper: Weak<dyn InetSocket>,
    ) -> Result<Self, SystemError> {
        Ok(Self {
            state: Arc::try_new(RegistrationState {
                wrapper,
                publication: Mutex::new(Publication {
                    reserved: Some(reserved),
                    published: None,
                    cancelled: false,
                }),
            })
            .map_err(|_| SystemError::ENOMEM)?,
            retained: false,
        })
    }
    pub(super) fn publisher(&self) -> ConnectingRegistrationPublisher {
        ConnectingRegistrationPublisher(self.state.clone())
    }
    pub(super) fn retain(&mut self) -> ConnectingRegistrationLease {
        self.retained = true;
        ConnectingRegistrationLease(self.state.clone())
    }
    pub(super) fn cancel(&self) {
        self.state.cancel();
    }
}
impl RegistrationState {
    fn cancel(&self) {
        let (reserved, published) = {
            let mut state = self.publication.lock();
            state.cancelled = true;
            (state.reserved.take(), state.published.take())
        };
        // Exact slot/id cannot remove a newer attempt; last wrapper retirement
        // occurs outside the publication guard.
        drop(reserved);
        if let Some(published) = published {
            published.cancel();
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
    pub(super) fn publish(self) {
        let Some(wrapper) = self.0.wrapper.upgrade() else {
            self.0.cancel();
            return;
        };
        let mut state = self.0.publication.lock();
        if !state.cancelled {
            if let Some(reserved) = state.reserved.take() {
                // State→registry short commit, no growth/resource callbacks.
                state.published = reserved.publish(wrapper.clone());
            }
        }
        drop(state);
        drop(wrapper);
    }
}
impl ConnectingRegistrationLease {
    pub(super) fn cancel(&self) {
        self.0.cancel();
    }
}
