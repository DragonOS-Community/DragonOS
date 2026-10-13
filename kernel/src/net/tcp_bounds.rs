//! Strong notification ownership with pre-reserved, physically accounted slots.
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
    },
    libs::rwlock::RwLock,
    mm::allocation::MemoryAllocationError,
    net::socket::inet::InetSocket,
};

pub(crate) const BOUND_BATCH: usize = 32;

/// Protocol polling cannot acknowledge task-only listener stock work. One
/// namespace task walks a captured finite range, sixteen slots per iteration.
pub(crate) struct RefillRequests {
    requested: AtomicU64,
    completed: AtomicU64,
    sweep: crate::libs::mutex::Mutex<RefillSweep>,
}

struct RefillSweep {
    epoch: u64,
    end: usize,
    cursor: usize,
}
pub(crate) struct RefillBatch {
    pub start: usize,
    pub end: usize,
    epoch: u64,
    last: bool,
}

impl RefillRequests {
    pub(crate) fn new() -> Self {
        Self {
            requested: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            sweep: crate::libs::mutex::Mutex::new(RefillSweep {
                epoch: 0,
                end: 0,
                cursor: 0,
            }),
        }
    }
    pub(crate) fn request(&self) {
        self.requested.fetch_add(1, Ordering::Release);
    }
    pub(crate) fn pending(&self) -> bool {
        self.requested.load(Ordering::Acquire) != self.completed.load(Ordering::Acquire)
    }
    pub(crate) fn next_batch(&self, current_end: usize) -> Option<RefillBatch> {
        let mut sweep = self.sweep.lock();
        if sweep.end == 0 {
            let epoch = self.requested.load(Ordering::Acquire);
            if epoch == self.completed.load(Ordering::Acquire) {
                return None;
            }
            if current_end == 0 {
                self.completed.store(epoch, Ordering::Release);
                return None;
            }
            sweep.epoch = epoch;
            sweep.end = current_end;
            sweep.cursor = 0;
        }
        let start = sweep.cursor;
        let end = start.saturating_add(16).min(sweep.end);
        sweep.cursor = end;
        Some(RefillBatch {
            start,
            end,
            epoch: sweep.epoch,
            last: end == sweep.end,
        })
    }
    pub(crate) fn finish_batch(&self, batch: RefillBatch) {
        if batch.last {
            let mut sweep = self.sweep.lock();
            debug_assert_eq!(batch.epoch, sweep.epoch);
            // A request arriving at an already-visited slot remains pending:
            // only this sweep's captured epoch, never the latest, is completed.
            self.completed.store(batch.epoch, Ordering::Release);
            sweep.end = 0;
        }
    }
}

enum Slot {
    Vacant,
    Reserved(u64),
    Bound(u64, Arc<dyn InetSocket>),
}

struct Storage {
    // The array physically dies before its receipt. Vacancies retain capacity.
    slots: Vec<Slot>,
    _charge: Option<ObjectCharge>,
}

impl Storage {
    fn prepare(
        creator: &Arc<ObjectAccount>,
        capacity: usize,
    ) -> Result<Self, MemoryAllocationError> {
        let layout =
            core::alloc::Layout::array::<Slot>(capacity).map_err(|_| SystemError::EOVERFLOW)?;
        let charge = creator.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        for _ in 0..capacity {
            slots.push(Slot::Vacant);
        }
        Ok(Self {
            slots,
            _charge: Some(charge),
        })
    }
}

pub(crate) struct BoundRegistry {
    storage: RwLock<Storage>,
    next_id: AtomicU64,
    bound_count: AtomicUsize,
}

impl BoundRegistry {
    pub(crate) fn new() -> Self {
        Self {
            storage: RwLock::new(Storage {
                slots: Vec::new(),
                _charge: None,
            }),
            next_id: AtomicU64::new(1),
            bound_count: AtomicUsize::new(0),
        }
    }

    fn next_id(&self) -> Result<u64, SystemError> {
        self.next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| SystemError::EOVERFLOW)
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<BoundSlotReservation, MemoryAllocationError> {
        let id = self.next_id()?;
        loop {
            let old_len = {
                let mut storage = self.storage.write();
                if let Some(index) = storage
                    .slots
                    .iter()
                    .position(|slot| matches!(slot, Slot::Vacant))
                {
                    storage.slots[index] = Slot::Reserved(id);
                    return Ok(BoundSlotReservation {
                        registry: self.clone(),
                        index,
                        id,
                        active: true,
                    });
                }
                storage.slots.len()
            };
            let capacity = if old_len == 0 {
                4
            } else {
                old_len.checked_mul(2).ok_or(SystemError::EOVERFLOW)?
            };
            let prepared = Storage::prepare(creator, capacity);
            let mut storage = self.storage.write();
            // A competitor may have closed a socket while preparation failed.
            if let Some(index) = storage
                .slots
                .iter()
                .position(|slot| matches!(slot, Slot::Vacant))
            {
                storage.slots[index] = Slot::Reserved(id);
                drop(storage);
                drop(prepared);
                return Ok(BoundSlotReservation {
                    registry: self.clone(),
                    index,
                    id,
                    active: true,
                });
            }
            let mut prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    drop(storage);
                    return Err(error);
                }
            };
            if storage.slots.len() > capacity {
                drop(storage);
                drop(prepared);
                continue;
            }
            let index = storage.slots.len();
            for (old, new) in storage.slots.iter_mut().zip(prepared.slots.iter_mut()) {
                *new = core::mem::replace(old, Slot::Vacant);
            }
            // Equal capacity can occur only after another grow. Its full table
            // needs a larger candidate, never an out-of-bounds reservation.
            if index == capacity {
                for (old, new) in storage.slots.iter_mut().zip(prepared.slots.iter_mut()) {
                    *old = core::mem::replace(new, Slot::Vacant);
                }
                drop(storage);
                drop(prepared);
                continue;
            }
            prepared.slots[index] = Slot::Reserved(id);
            let retired = core::mem::replace(&mut *storage, prepared);
            drop(storage);
            drop(retired);
            return Ok(BoundSlotReservation {
                registry: self.clone(),
                index,
                id,
                active: true,
            });
        }
    }

    pub(crate) fn publish(&self, index: usize, id: u64, socket: Arc<dyn InetSocket>) -> bool {
        let mut storage = self.storage.write();
        assert!(matches!(storage.slots.get(index), Some(Slot::Reserved(actual)) if *actual == id));
        let already_bound = storage
            .slots
            .iter()
            .any(|slot| matches!(slot, Slot::Bound(_, bound) if Arc::ptr_eq(bound, &socket)));
        if already_bound {
            storage.slots[index] = Slot::Vacant;
        } else {
            storage.slots[index] = Slot::Bound(id, socket.clone());
            self.bound_count.fetch_add(1, Ordering::Release);
        }
        // The input pin is retired after the registry guard, including the
        // idempotent case where this is its last otherwise-live reference.
        drop(storage);
        drop(socket);
        !already_bound
    }

    pub(crate) fn cancel(&self, index: usize, id: u64) {
        let mut storage = self.storage.write();
        if matches!(storage.slots.get(index), Some(Slot::Reserved(actual)) if *actual == id) {
            storage.slots[index] = Slot::Vacant;
        }
    }

    pub(crate) fn unpublish(&self, index: usize, id: u64) {
        let retired = {
            let mut storage = self.storage.write();
            if matches!(storage.slots.get(index), Some(Slot::Bound(actual, _)) if *actual == id) {
                self.bound_count.fetch_sub(1, Ordering::Release);
                Some(core::mem::replace(&mut storage.slots[index], Slot::Vacant))
            } else {
                None
            }
        };
        drop(retired);
    }

    pub(crate) fn take_bound(&self, socket: &Arc<dyn InetSocket>) -> Option<Arc<dyn InetSocket>> {
        let retired = {
            let mut storage = self.storage.write();
            storage
                .slots
                .iter()
                .position(
                    |slot| matches!(slot, Slot::Bound(_, bound) if Arc::ptr_eq(bound, socket)),
                )
                .map(|index| {
                    self.bound_count.fetch_sub(1, Ordering::Release);
                    core::mem::replace(&mut storage.slots[index], Slot::Vacant)
                })
        };
        match retired {
            Some(Slot::Bound(_, socket)) => Some(socket),
            None => None,
            _ => unreachable!("only a published bound slot is removed"),
        }
    }

    /// Reuse the original physical notification slot for an explicit return.
    pub(crate) fn take_bound_reserved(
        self: &Arc<Self>,
        socket: &Arc<dyn InetSocket>,
    ) -> Option<(BoundSlotReservation, Arc<dyn InetSocket>)> {
        let mut storage = self.storage.write();
        let index = storage
            .slots
            .iter()
            .position(|slot| matches!(slot, Slot::Bound(_, bound) if Arc::ptr_eq(bound, socket)))?;
        let Slot::Bound(id, strong) = core::mem::replace(&mut storage.slots[index], Slot::Vacant)
        else {
            unreachable!()
        };
        storage.slots[index] = Slot::Reserved(id);
        self.bound_count.fetch_sub(1, Ordering::Release);
        Some((
            BoundSlotReservation {
                registry: self.clone(),
                index,
                id,
                active: true,
            },
            strong,
        ))
    }

    pub(crate) fn unbind(&self, socket: &Arc<dyn InetSocket>) {
        drop(self.take_bound(socket));
    }

    pub(crate) fn count(&self) -> usize {
        self.bound_count.load(Ordering::Acquire)
    }

    pub(crate) fn count_atomic(&self) -> &AtomicUsize {
        &self.bound_count
    }

    pub(crate) fn end(&self) -> usize {
        self.storage.read().slots.len()
    }

    pub(crate) fn snapshot(
        &self,
        start: usize,
        end: usize,
    ) -> [Option<Arc<dyn InetSocket>>; BOUND_BATCH] {
        let mut pins = core::array::from_fn(|_| None);
        let storage = self.storage.read_irqsave();
        for (offset, pin) in pins.iter_mut().enumerate() {
            let Some(index) = start.checked_add(offset).filter(|index| *index < end) else {
                break;
            };
            if let Some(Slot::Bound(_, socket)) = storage.slots.get(index) {
                *pin = Some(socket.clone());
            }
        }
        pins
    }
}

pub(crate) struct BoundSlotReservation {
    registry: Arc<BoundRegistry>,
    index: usize,
    id: u64,
    active: bool,
}

impl core::fmt::Debug for BoundSlotReservation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundSlotReservation")
            .field("index", &self.index)
            .field("id", &self.id)
            .finish()
    }
}

impl BoundSlotReservation {
    pub(crate) fn publish(mut self, socket: Arc<dyn InetSocket>) -> Option<PublishedBoundSlot> {
        let created = self.registry.publish(self.index, self.id, socket);
        self.active = false;
        created.then(|| PublishedBoundSlot {
            registry: self.registry.clone(),
            index: self.index,
            id: self.id,
        })
    }
}

pub(crate) struct PublishedBoundSlot {
    registry: Arc<BoundRegistry>,
    index: usize,
    id: u64,
}
impl PublishedBoundSlot {
    pub(crate) fn cancel(self) {
        self.registry.unpublish(self.index, self.id);
    }
}

impl Drop for BoundSlotReservation {
    fn drop(&mut self) {
        if self.active {
            self.registry.cancel(self.index, self.id);
        }
    }
}

impl core::fmt::Debug for PublishedBoundSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PublishedBoundSlot")
            .field("index", &self.index)
            .field("id", &self.id)
            .finish()
    }
}
