//! Set-local undo identities and their dense association slots.
use super::{SemUndoAssociation, SemUndoGroup};
use alloc::{sync::Arc, vec::Vec};
use hashbrown::HashMap;
use system_error::SystemError;

#[derive(Debug, Default)]
pub(in crate::ipc::sem) struct SemUndoRegistry {
    entries: Vec<SemUndoAssociation>,
    // Each live entry owns a Weak that pins the allocation used as its key.
    // The address cannot be reused until both the entry and index are removed.
    index: HashMap<usize, usize>,
}

impl SemUndoRegistry {
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(in crate::ipc::sem) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    fn can_hold(&self, required: usize) -> bool {
        self.entries.capacity() >= required && self.index.capacity() >= required
    }

    /// Prepare empty spare storage outside the manager lock.
    pub(in crate::ipc::sem) fn prepare(&mut self, required: usize) -> Result<(), SystemError> {
        debug_assert!(self.is_empty() && self.index.is_empty());
        self.entries
            .try_reserve_exact(required)
            .map_err(|_| SystemError::ENOMEM)?;
        self.index
            .try_reserve(required)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(())
    }

    pub(super) fn iter(&self) -> core::slice::Iter<'_, SemUndoAssociation> {
        self.entries.iter()
    }

    /// Only RMID may turn live slots into deferred cleanup, after removing the set.
    pub(super) fn iter_mut(&mut self) -> core::slice::IterMut<'_, SemUndoAssociation> {
        self.entries.iter_mut()
    }

    fn push_prepared(&mut self, association: SemUndoAssociation) {
        assert!(self.can_hold(self.len() + 1));
        let key = association.group().as_ptr() as usize;
        debug_assert!(!self.index.contains_key(&key));
        self.index.insert(key, self.len());
        self.entries.push(association);
    }

    fn remove(&mut self, key: usize) {
        let Some(slot) = self.index.remove(&key) else {
            return;
        };
        let removed = self.entries.swap_remove(slot);
        if let Some(moved) = self.entries.get(slot) {
            *self
                .index
                .get_mut(&(moved.group().as_ptr() as usize))
                .expect("live undo association is indexed") = slot;
        }
        drop(removed);
    }

    pub(super) fn compact(&mut self) {
        let mut slot = 0;
        while let Some(entry) = self.entries.get(slot) {
            if entry.group().strong_count() == 0 {
                self.remove(entry.group().as_ptr() as usize);
            } else {
                slot += 1;
            }
        }
    }

    fn install_prepared(&mut self, spare: &mut Self) {
        assert!(spare.is_empty() && spare.can_hold(self.len()));
        for entry in self.entries.drain(..) {
            spare.push_prepared(entry);
        }
        self.index.clear();
        core::mem::swap(self, spare);
    }

    pub(super) fn register_prepared(
        &mut self,
        group: &Arc<SemUndoGroup>,
        spare: &mut Self,
    ) -> Result<(), usize> {
        debug_assert!(spare.is_empty());
        if self.index.contains_key(&(Arc::as_ptr(group) as usize)) {
            return Ok(());
        }
        if !self.can_hold(self.len() + 1) {
            self.compact();
        }
        if !self.can_hold(self.len() + 1) {
            if !spare.can_hold(self.len() + 1) {
                // Rebuild at the dense capacity for hash tombstones; grow
                // geometrically only when the dense storage is actually full.
                return Err(if self.len() == self.capacity() {
                    self.len().saturating_mul(2).max(4)
                } else {
                    self.capacity()
                });
            }
            self.install_prepared(spare);
        }
        self.push_prepared(SemUndoAssociation::Group(Arc::downgrade(group)));
        Ok(())
    }

    pub(super) fn unregister(&mut self, group: &Arc<SemUndoGroup>) {
        self.remove(Arc::as_ptr(group) as usize);
    }

    /// Recheck unlocked preparation; return the next capacity request, if any.
    /// Both replaced allocations remain caller-owned for unlocked disposal.
    pub(super) fn shrink_prepared(&mut self, spare: &mut Self, retired: &mut Self) -> usize {
        debug_assert!(spare.is_empty());
        debug_assert!(retired.is_empty() && retired.capacity() == 0);
        let len = self.len();
        if len == 0 {
            core::mem::swap(self, retired);
            return 0;
        }
        if self.capacity() <= 4 || len > self.capacity() / 4 {
            return 0;
        }
        if !spare.can_hold(len) {
            return len.saturating_mul(2).max(4);
        }
        if spare.capacity() < self.capacity() {
            self.install_prepared(spare);
        }
        0
    }
}
