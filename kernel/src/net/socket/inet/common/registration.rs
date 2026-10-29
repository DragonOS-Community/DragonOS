//! Pre-born interface protocol and notification vacancies.
//!
//! Preparation is a task operation. Commit borrows this owner so both rejected
//! candidates and retired registry/set identities remain outside resource
//! guards. A device move cannot recycle a reservation in another table.
use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    cgroup::memory::object::ObjectAccount,
    driver::net::InterfaceSocketSet,
    mm::allocation::MemoryAllocationError,
    net::{
        socket::inet::InetSocket,
        tcp_bounds::{BoundRegistry, BoundSlotReservation},
        Iface,
    },
    process::namespace::net_namespace::NetNamespace,
};

use super::BoundInner;

pub(crate) struct PreparedIfaceSocket {
    // Cancel the exact vacancy before retiring its backing identity.
    slot: Option<smoltcp::iface::SocketHandle>,
    notification: Option<BoundSlotReservation>,
    set: InterfaceSocketSet,
    registry: Arc<BoundRegistry>,
    iface: Arc<dyn Iface>,
    netns: Arc<NetNamespace>,
    epoch: u64,
}

impl PreparedIfaceSocket {
    pub(crate) fn try_new(
        iface: Arc<dyn Iface>,
        netns: Arc<NetNamespace>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        let (set, registry, epoch) = iface.common().socket_registration_snapshot(&netns)?;
        let notification = registry.reserve(creator)?;
        let mut prepared = Self {
            slot: None,
            notification: Some(notification),
            set,
            registry,
            iface,
            netns,
            epoch,
        };
        loop {
            let old_slots = {
                let set = prepared.set.clone();
                let mut sockets = set.lock();
                if let Some(slot) = sockets.reserve_slot_no_grow() {
                    prepared.slot = Some(slot);
                    return Ok(prepared);
                }
                sockets.storage_slots()
            };
            let capacity = if old_slots == 0 {
                4
            } else {
                old_slots.checked_mul(2).ok_or(SystemError::EOVERFLOW)?
            };
            let candidate = crate::net::socket::buffer::try_socket_set_storage(creator, capacity);
            let mut sockets = prepared.set.lock();
            // Preparation may fail while another CPU closes a protocol.
            if let Some(slot) = sockets.reserve_slot_no_grow() {
                prepared.slot = Some(slot);
                drop(sockets);
                drop(candidate);
                return Ok(prepared);
            }
            let candidate = match candidate {
                Ok(candidate) => candidate,
                Err(error) => {
                    drop(sockets);
                    return Err(error);
                }
            };
            if sockets.storage_slots() > capacity {
                drop(sockets);
                drop(candidate);
                continue;
            }
            let retired = match sockets.replace_prepared_storage(candidate) {
                Ok(retired) => retired,
                Err(_) => unreachable!("empty prepared array covers the unchanged table"),
            };
            drop(sockets);
            drop(retired);
        }
    }

    /// Hold the existing atomic device admission until protocol/port/Strong
    /// publication is complete. No allocation or quota recovery belongs here.
    pub(crate) fn with_commit<R>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, SystemError> {
        let iface = self.iface.clone();
        let common = iface.common();
        let admission = common
            .enter_socket_publication(self.epoch)
            .ok_or(SystemError::ENODEV)?;
        if !common.socket_registration_is_current(
            &self.netns,
            &self.set,
            &self.registry,
            self.epoch,
        ) {
            drop(admission);
            return Err(SystemError::ENODEV);
        }
        let result = operation(self);
        drop(admission);
        Ok(result)
    }

    pub(crate) fn attach<T: smoltcp::socket::AnySocket<'static>>(
        &mut self,
        socket: T,
    ) -> BoundInner {
        let handle = self.slot.take().expect("protocol vacancy consumed once");
        self.set.lock().add_reserved(handle, socket);
        BoundInner::from_registered(
            handle,
            self.iface.clone(),
            self.set.clone(),
            self.netns.clone(),
            Arc::downgrade(&self.registry),
        )
    }

    pub(crate) fn publish(&mut self, socket: Arc<dyn InetSocket>) {
        let reserved = self
            .notification
            .take()
            .expect("notification vacancy consumed once");
        // The prepared owner retains registry identity while this extra pin is
        // retired. The Strong socket remains solely in the published slot.
        drop(reserved.publish(socket));
    }

    pub(crate) fn iface(&self) -> &Arc<dyn Iface> {
        &self.iface
    }
    pub(crate) fn socket_set(&self) -> &InterfaceSocketSet {
        &self.set
    }

    pub(crate) fn take_slot(&mut self) -> smoltcp::iface::SocketHandle {
        self.slot.take().expect("protocol vacancy consumed once")
    }

    /// Move the actual protocol into both pre-born target vacancies. The
    /// returned owner pins the original vacancies for an allocation-free return.
    pub(crate) fn move_bound(
        &mut self,
        bound: &mut BoundInner,
        socket: Arc<dyn InetSocket>,
    ) -> Result<Option<IfaceSocketReturn>, SystemError> {
        if Arc::ptr_eq(&bound.socket_set, &self.set) {
            return Ok(None);
        }
        // The source may already be permanently retired. We remove only from
        // the exact pinned set, never publish into its device's new current
        // holder. Set mutexes serialize all actual protocol borrows; requiring
        // the source's permanently closed admission would prevent evacuation.
        let result = self.with_commit(|target| {
            let source_registry = bound.registry.upgrade();
            let notification = source_registry
                .as_ref()
                .and_then(|registry| registry.take_bound_reserved(&socket));
            let old = BoundInner::from_registered(
                bound.handle,
                bound.iface.clone(),
                bound.socket_set.clone(),
                bound.netns.clone(),
                bound.registry.clone(),
            );
            let handle = target.take_slot();
            if Arc::as_ptr(&old.socket_set) < Arc::as_ptr(&target.set) {
                let mut source = old.socket_set.lock();
                let mut destination = target.set.lock();
                source.reserve_existing(old.handle);
                let protocol = source.remove(old.handle);
                destination.add_reserved_socket(handle, protocol);
            } else {
                let mut destination = target.set.lock();
                let mut source = old.socket_set.lock();
                source.reserve_existing(old.handle);
                let protocol = source.remove(old.handle);
                destination.add_reserved_socket(handle, protocol);
            }
            *bound = BoundInner::from_registered(
                handle,
                target.iface.clone(),
                target.set.clone(),
                target.netns.clone(),
                Arc::downgrade(&target.registry),
            );
            target.publish(socket.clone());
            IfaceSocketReturn {
                old: Some(old),
                notification,
                socket,
                source_registry,
            }
        });
        result.map(Some)
    }
}

/// A temporary move retains the actual original slots, not spare capacity.
/// Drop cancels empty slots only; restoring is always an explicit operation.
pub(crate) struct IfaceSocketReturn {
    old: Option<BoundInner>,
    notification: Option<(BoundSlotReservation, Arc<dyn InetSocket>)>,
    socket: Arc<dyn InetSocket>,
    // Even an already-unregistered old registry must not lose its final pin
    // under the caller's placement/inner guard after Weak::upgrade.
    source_registry: Option<Arc<BoundRegistry>>,
}
impl IfaceSocketReturn {
    pub(crate) fn restore(&mut self, bound: &mut BoundInner) -> RetiredIfaceBinding {
        // Return to the retained exact slot, even if either device has been
        // removed or moved meanwhile. This does not publish into a new current
        // holder, and requires no admission to permanently retired devices.
        let old = self.old.take().expect("return consumed once");
        let target_registry = bound.registry.upgrade();
        let retired = target_registry
            .as_ref()
            .and_then(|registry| registry.take_bound(&self.socket));
        // Remove target Strong before publishing source, including same-registry.
        if Arc::as_ptr(&old.socket_set) < Arc::as_ptr(&bound.socket_set) {
            let mut source = old.socket_set.lock();
            let mut target = bound.socket_set.lock();
            source.add_reserved_socket(old.handle, target.remove(bound.handle));
        } else {
            let mut target = bound.socket_set.lock();
            let mut source = old.socket_set.lock();
            source.add_reserved_socket(old.handle, target.remove(bound.handle));
        }
        if let Some((reserved, socket)) = self.notification.take() {
            drop(reserved.publish(socket));
        }
        *bound = old;
        RetiredIfaceBinding {
            socket: retired,
            registry: target_registry,
        }
    }
}
impl Drop for IfaceSocketReturn {
    fn drop(&mut self) {
        if let Some(old) = self.old.take() {
            old.socket_set.lock().cancel_reserved(old.handle);
        }
    }
}

impl Drop for PreparedIfaceSocket {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.set.lock().cancel_reserved(slot);
        }
    }
}

/// Take ownership under short registry locks; retire the last socket and the
/// potentially retired registry only after the caller's placement/inner locks.
pub(crate) struct RetiredIfaceBinding {
    pub(crate) socket: Option<Arc<dyn InetSocket>>,
    pub(crate) registry: Option<Arc<BoundRegistry>>,
}
