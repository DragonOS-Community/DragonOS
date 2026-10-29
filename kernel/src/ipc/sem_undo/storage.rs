//! Dense replay order with a full-ID lookup index. All index maintenance lives here.
use super::SemUndoRecord;
use crate::ipc::sem::SemId;
use alloc::vec::Vec;
use hashbrown::HashMap;
use system_error::SystemError;

#[derive(Debug, Default)]
pub(super) struct UndoRecords {
    records: Vec<SemUndoRecord>,
    index: HashMap<SemId, usize>,
}

impl UndoRecords {
    pub(super) fn len(&self) -> usize {
        self.records.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Physical dense capacity drives reclamation; hash capacity can decrease
    /// after deletion due to tombstones, without releasing its allocation.
    pub(super) fn capacity(&self) -> usize {
        self.records.capacity()
    }

    pub(super) fn can_hold(&self, required: usize) -> bool {
        self.records.capacity() >= required && self.index.capacity() >= required
    }

    /// Only call on empty spare storage outside manager/group locks.
    pub(super) fn prepare(&mut self, required: usize) -> Result<(), SystemError> {
        debug_assert!(self.is_empty() && self.index.is_empty());
        self.records
            .try_reserve_exact(required)
            .map_err(|_| SystemError::ENOMEM)?;
        self.index
            .try_reserve(required)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(())
    }

    pub(super) fn get(&self, semid: SemId) -> Option<&SemUndoRecord> {
        self.index.get(&semid).map(|&slot| &self.records[slot])
    }

    pub(super) fn get_mut(&mut self, semid: SemId) -> Option<&mut SemUndoRecord> {
        let slot = *self.index.get(&semid)?;
        Some(&mut self.records[slot])
    }

    /// The caller owns a reservation covering both allocations.
    pub(super) fn push_prepared(&mut self, record: SemUndoRecord) {
        assert!(self.can_hold(self.len() + 1));
        debug_assert!(!self.index.contains_key(&record.semid));
        self.index.insert(record.semid, self.records.len());
        self.records.push(record);
    }

    pub(super) fn remove(&mut self, semid: SemId) -> Option<SemUndoRecord> {
        let slot = self.index.remove(&semid)?;
        let removed = self.records.swap_remove(slot);
        if let Some(moved) = self.records.get(slot) {
            *self
                .index
                .get_mut(&moved.semid)
                .expect("live undo record is indexed") = slot;
        }
        Some(removed)
    }

    pub(super) fn pop(&mut self) -> Option<SemUndoRecord> {
        let record = self.records.pop()?;
        self.index.remove(&record.semid);
        Some(record)
    }

    /// Both capacities must already cover live records AND pending reservations.
    /// Keep the old allocations in spare so the caller can dispose of them unlocked.
    pub(super) fn install_prepared(&mut self, spare: &mut Self, required: usize) {
        assert!(spare.is_empty() && spare.index.is_empty() && spare.can_hold(required));
        assert!(required >= self.len());
        for record in self.records.drain(..) {
            spare.push_prepared(record);
        }
        self.index.clear();
        core::mem::swap(self, spare);
    }
}
