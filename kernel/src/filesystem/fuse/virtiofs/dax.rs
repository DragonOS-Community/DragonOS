use alloc::{collections::VecDeque, vec::Vec};

use log::error;
use system_error::SystemError;

use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};

pub(crate) const DAX_RANGE_SIZE: usize = 2 * 1024 * 1024;

#[derive(Debug)]
enum DaxRangeState {
    Free,
    Reserved { nonce: u64 },
    InodeOwned { inode: u64, refs: u64 },
    Reclaiming { inode: u64, nonce: u64 },
    Retired,
}

#[derive(Debug)]
struct DaxRangeEntry {
    generation: u64,
    transition_nonce: u64,
    validation_epoch: u64,
    state: DaxRangeState,
}

#[derive(Debug)]
struct DaxRangeAllocatorState {
    entries: Vec<DaxRangeEntry>,
    free: VecDeque<usize>,
    reclaim_cursor: usize,
    validation_epoch: u64,
    shutdown: bool,
}

#[derive(Debug)]
pub(crate) struct DaxRangeAllocator {
    state: SpinLock<DaxRangeAllocatorState>,
    wait: WaitQueue,
}

#[derive(Debug, PartialEq, Eq)]
#[must_use = "a DAX reservation must be assigned or cancelled"]
pub(crate) struct AllocationToken {
    index: usize,
    generation: u64,
    nonce: u64,
}

#[derive(Debug, PartialEq, Eq)]
#[must_use = "an owned DAX token identifies a live inode mapping"]
pub(crate) struct OwnedToken {
    index: usize,
    generation: u64,
    inode: u64,
}

#[derive(Debug, PartialEq, Eq)]
#[must_use = "a reclaim candidate must be revalidated before use"]
pub(crate) struct ReclaimCandidate {
    index: usize,
    generation: u64,
    inode: u64,
}

#[derive(Debug, PartialEq, Eq)]
#[must_use = "an isolated DAX range must finish or cancel reclaim"]
pub(crate) struct ReclaimToken {
    index: usize,
    generation: u64,
    inode: u64,
    nonce: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DaxAllocatorSnapshot {
    pub(crate) total: usize,
    pub(crate) free: usize,
    pub(crate) reserved: usize,
    pub(crate) inode_owned: usize,
    pub(crate) reclaiming: usize,
    pub(crate) retired: usize,
    pub(crate) shutdown: bool,
}

impl AllocationToken {
    pub(crate) fn window_offset(&self) -> usize {
        self.index * DAX_RANGE_SIZE
    }

    pub(crate) fn len(&self) -> usize {
        DAX_RANGE_SIZE
    }
}

impl OwnedToken {
    pub(crate) fn window_offset(&self) -> usize {
        self.index * DAX_RANGE_SIZE
    }

    pub(crate) fn len(&self) -> usize {
        DAX_RANGE_SIZE
    }

    pub(crate) fn inode(&self) -> u64 {
        self.inode
    }
}

impl ReclaimCandidate {
    pub(crate) fn window_offset(&self) -> usize {
        self.index * DAX_RANGE_SIZE
    }

    pub(crate) fn inode(&self) -> u64 {
        self.inode
    }
}

impl DaxRangeAllocatorState {
    fn snapshot(&self) -> DaxAllocatorSnapshot {
        let mut snapshot = DaxAllocatorSnapshot {
            total: self.entries.len(),
            shutdown: self.shutdown,
            ..Default::default()
        };
        for entry in &self.entries {
            match entry.state {
                DaxRangeState::Free => snapshot.free += 1,
                DaxRangeState::Reserved { .. } => snapshot.reserved += 1,
                DaxRangeState::InodeOwned { .. } => snapshot.inode_owned += 1,
                DaxRangeState::Reclaiming { .. } => snapshot.reclaiming += 1,
                DaxRangeState::Retired => snapshot.retired += 1,
            }
        }
        snapshot
    }

    fn invariants_hold(&mut self) -> bool {
        let snapshot = self.snapshot();
        if snapshot.free
            + snapshot.reserved
            + snapshot.inode_owned
            + snapshot.reclaiming
            + snapshot.retired
            != snapshot.total
            || snapshot.free != self.free.len()
        {
            return false;
        }

        let epoch = match self.validation_epoch.checked_add(1) {
            Some(epoch) => epoch,
            None => {
                for entry in &mut self.entries {
                    entry.validation_epoch = 0;
                }
                1
            }
        };
        self.validation_epoch = epoch;
        for index in &self.free {
            let Some(entry) = self.entries.get_mut(*index) else {
                return false;
            };
            if !matches!(entry.state, DaxRangeState::Free) || entry.validation_epoch == epoch {
                return false;
            }
            entry.validation_epoch = epoch;
        }
        for entry in &self.entries {
            if matches!(entry.state, DaxRangeState::Free) != (entry.validation_epoch == epoch) {
                return false;
            }
        }
        true
    }
}

impl DaxRangeAllocator {
    pub(crate) fn new(window_len: usize) -> Result<Self, SystemError> {
        let range_count = window_len / DAX_RANGE_SIZE;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(range_count)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut free = VecDeque::new();
        free.try_reserve_exact(range_count)
            .map_err(|_| SystemError::ENOMEM)?;

        for index in 0..range_count {
            entries.push(DaxRangeEntry {
                generation: 0,
                transition_nonce: 0,
                validation_epoch: 0,
                state: DaxRangeState::Free,
            });
            free.push_back(index);
        }

        Ok(Self {
            state: SpinLock::new(DaxRangeAllocatorState {
                entries,
                free,
                reclaim_cursor: 0,
                validation_epoch: 0,
                shutdown: false,
            }),
            wait: WaitQueue::default(),
        })
    }

    pub(crate) fn try_allocate(&self) -> Result<AllocationToken, SystemError> {
        let mut state = self.state.lock_irqsave();
        if state.shutdown {
            return Err(SystemError::ENODEV);
        }

        while let Some(index) = state.free.pop_front() {
            let entry = &mut state.entries[index];
            debug_assert!(matches!(entry.state, DaxRangeState::Free));
            let Some(generation) = entry.generation.checked_add(1) else {
                entry.state = DaxRangeState::Retired;
                continue;
            };
            let Some(nonce) = entry.transition_nonce.checked_add(1) else {
                entry.state = DaxRangeState::Retired;
                continue;
            };
            entry.generation = generation;
            entry.transition_nonce = nonce;
            entry.state = DaxRangeState::Reserved { nonce };
            return Ok(AllocationToken {
                index,
                generation,
                nonce,
            });
        }
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    pub(crate) fn wait_available_interruptible(&self) -> Result<(), SystemError> {
        self.wait.wait_until_interruptible(|| {
            let state = self.state.lock_irqsave();
            if state.shutdown {
                Some(Err(SystemError::ENODEV))
            } else if !state.free.is_empty() {
                Some(Ok(()))
            } else {
                None
            }
        })?
    }

    pub(crate) fn assign_inode(
        &self,
        token: &AllocationToken,
        inode: u64,
    ) -> Result<OwnedToken, SystemError> {
        if inode == 0 {
            return Err(SystemError::EINVAL);
        }
        let mut state = self.state.lock_irqsave();
        if state.shutdown {
            return Err(SystemError::ENODEV);
        }
        let entry = state
            .entries
            .get_mut(token.index)
            .ok_or(SystemError::EINVAL)?;
        if entry.generation != token.generation
            || !matches!(entry.state, DaxRangeState::Reserved { nonce } if nonce == token.nonce)
        {
            return Err(SystemError::EINVAL);
        }
        entry.state = DaxRangeState::InodeOwned { inode, refs: 1 };
        Ok(OwnedToken {
            index: token.index,
            generation: token.generation,
            inode,
        })
    }

    pub(crate) fn cancel_reservation(&self, token: &AllocationToken) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        let entry = state
            .entries
            .get_mut(token.index)
            .ok_or(SystemError::EINVAL)?;
        if entry.generation != token.generation
            || !matches!(entry.state, DaxRangeState::Reserved { nonce } if nonce == token.nonce)
        {
            return Err(SystemError::EINVAL);
        }
        entry.state = DaxRangeState::Free;
        state.free.push_back(token.index);
        drop(state);
        self.wait.wakeup(None);
        Ok(())
    }

    pub(crate) fn get(&self, token: &OwnedToken) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        if state.shutdown {
            return Err(SystemError::ENODEV);
        }
        let entry = state
            .entries
            .get_mut(token.index)
            .ok_or(SystemError::EINVAL)?;
        if entry.generation != token.generation {
            return Err(SystemError::EINVAL);
        }
        let DaxRangeState::InodeOwned { inode, refs } = &mut entry.state else {
            return Err(SystemError::EINVAL);
        };
        if *inode != token.inode {
            return Err(SystemError::EINVAL);
        }
        *refs = refs.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        Ok(())
    }

    pub(crate) fn put(&self, token: &OwnedToken) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        let entry = state
            .entries
            .get_mut(token.index)
            .ok_or(SystemError::EINVAL)?;
        if entry.generation != token.generation {
            return Err(SystemError::EINVAL);
        }
        let DaxRangeState::InodeOwned { inode, refs } = &mut entry.state else {
            return Err(SystemError::EINVAL);
        };
        if *inode != token.inode || *refs <= 1 {
            return Err(SystemError::EINVAL);
        }
        *refs -= 1;
        Ok(())
    }

    pub(crate) fn reclaim_candidates(
        &self,
        candidates: &mut Vec<ReclaimCandidate>,
        max: usize,
    ) -> Result<usize, SystemError> {
        candidates.clear();
        let limit = max.min(candidates.capacity());
        if limit == 0 {
            return Ok(0);
        }

        let mut state = self.state.lock_irqsave();
        let total = state.entries.len();
        if total == 0 {
            return Ok(0);
        }
        let start = state.reclaim_cursor % total;
        let mut scanned = 0usize;
        while scanned < total && candidates.len() < limit {
            let index = (start + scanned) % total;
            let entry = &state.entries[index];
            if let DaxRangeState::InodeOwned { inode, refs: 1 } = entry.state {
                candidates.push(ReclaimCandidate {
                    index,
                    generation: entry.generation,
                    inode,
                });
            }
            scanned += 1;
        }
        state.reclaim_cursor = (start + scanned) % total;
        Ok(candidates.len())
    }

    pub(crate) fn begin_reclaim(
        &self,
        candidate: &ReclaimCandidate,
    ) -> Result<ReclaimToken, SystemError> {
        let mut state = self.state.lock_irqsave();
        let entry = state
            .entries
            .get_mut(candidate.index)
            .ok_or(SystemError::EINVAL)?;
        if entry.generation != candidate.generation
            || !matches!(entry.state, DaxRangeState::InodeOwned { inode, refs: 1 } if inode == candidate.inode)
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let nonce = entry
            .transition_nonce
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        entry.transition_nonce = nonce;
        entry.state = DaxRangeState::Reclaiming {
            inode: candidate.inode,
            nonce,
        };
        Ok(ReclaimToken {
            index: candidate.index,
            generation: candidate.generation,
            inode: candidate.inode,
            nonce,
        })
    }

    pub(crate) fn finish_reclaim(&self, token: &ReclaimToken) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        Self::validate_reclaim_token(&state, &token)?;
        state.entries[token.index].state = DaxRangeState::Free;
        state.free.push_back(token.index);
        drop(state);
        self.wait.wakeup(None);
        Ok(())
    }

    pub(crate) fn cancel_reclaim(&self, token: &ReclaimToken) -> Result<OwnedToken, SystemError> {
        let mut state = self.state.lock_irqsave();
        Self::validate_reclaim_token(&state, &token)?;
        state.entries[token.index].state = DaxRangeState::InodeOwned {
            inode: token.inode,
            refs: 1,
        };
        Ok(OwnedToken {
            index: token.index,
            generation: token.generation,
            inode: token.inode,
        })
    }

    fn validate_reclaim_token(
        state: &DaxRangeAllocatorState,
        token: &ReclaimToken,
    ) -> Result<(), SystemError> {
        let entry = state.entries.get(token.index).ok_or(SystemError::EINVAL)?;
        if entry.generation != token.generation
            || !matches!(entry.state, DaxRangeState::Reclaiming { inode, nonce } if inode == token.inode && nonce == token.nonce)
        {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    pub(crate) fn begin_shutdown(&self) {
        let mut state = self.state.lock_irqsave();
        if state.shutdown {
            return;
        }
        state.shutdown = true;
        drop(state);
        self.wait.wakeup_all(None);
    }

    pub(crate) fn finish_shutdown(&self) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        let snapshot = state.snapshot();
        if !state.invariants_hold()
            || snapshot.reserved != 0
            || snapshot.inode_owned != 0
            || snapshot.reclaiming != 0
        {
            return Err(SystemError::EBUSY);
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> DaxAllocatorSnapshot {
        self.state.lock_irqsave().snapshot()
    }
}

impl Drop for DaxRangeAllocator {
    fn drop(&mut self) {
        let mut state = self.state.lock_irqsave();
        let snapshot = state.snapshot();
        if !state.invariants_hold()
            || snapshot.reserved != 0
            || snapshot.inode_owned != 0
            || snapshot.reclaiming != 0
        {
            error!(
                "virtiofs DAX allocator dropped with live ranges: {:?}",
                snapshot
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(allocator: &DaxRangeAllocator, inode: u64) -> OwnedToken {
        let reservation = allocator.try_allocate().unwrap();
        allocator.assign_inode(&reservation, inode).unwrap()
    }

    fn candidates(allocator: &DaxRangeAllocator, max: usize) -> Vec<ReclaimCandidate> {
        let mut out = Vec::with_capacity(max);
        allocator.reclaim_candidates(&mut out, max).unwrap();
        out
    }

    #[test]
    fn allocation_exhaustion_and_accounting() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE * 2 + 4096).unwrap();
        let first = allocator.try_allocate().unwrap();
        let second = allocator.try_allocate().unwrap();
        assert_eq!(first.window_offset(), 0);
        assert_eq!(second.window_offset(), DAX_RANGE_SIZE);
        assert_eq!(first.len(), DAX_RANGE_SIZE);
        assert_eq!(
            allocator.try_allocate(),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        );
        allocator.cancel_reservation(&first).unwrap();
        allocator.cancel_reservation(&second).unwrap();
        assert_eq!(allocator.snapshot().free, 2);
    }

    #[test]
    fn references_protect_mapping_from_reclaim() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let owned = assign(&allocator, 7);
        assert_eq!(owned.inode(), 7);
        assert_eq!(owned.window_offset(), 0);
        assert_eq!(owned.len(), DAX_RANGE_SIZE);
        allocator.get(&owned).unwrap();
        assert!(candidates(&allocator, 10).is_empty());
        allocator.put(&owned).unwrap();
        let candidate = candidates(&allocator, 10).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&candidate).unwrap();
        allocator.finish_reclaim(&reclaim).unwrap();
        assert_eq!(allocator.snapshot().free, 1);
    }

    #[test]
    fn reclaim_revalidates_and_can_be_cancelled() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let owned = assign(&allocator, 9);
        let stale = candidates(&allocator, 1).pop().unwrap();
        assert_eq!(stale.window_offset(), 0);
        assert_eq!(stale.inode(), 9);
        allocator.get(&owned).unwrap();
        assert_eq!(
            allocator.begin_reclaim(&stale),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        );
        allocator.put(&owned).unwrap();
        let candidate = candidates(&allocator, 1).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&candidate).unwrap();
        let owned = allocator.cancel_reclaim(&reclaim).unwrap();
        let candidate = candidates(&allocator, 1).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&candidate).unwrap();
        drop(owned);
        allocator.finish_reclaim(&reclaim).unwrap();
    }

    #[test]
    fn shutdown_is_deterministic() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let reservation = allocator.try_allocate().unwrap();
        allocator.begin_shutdown();
        assert_eq!(allocator.try_allocate(), Err(SystemError::ENODEV));
        assert_eq!(allocator.finish_shutdown(), Err(SystemError::EBUSY));
        allocator.cancel_reservation(&reservation).unwrap();
        allocator.finish_shutdown().unwrap();
        assert!(allocator.snapshot().shutdown);
    }

    #[test]
    fn zero_range_window_is_valid() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE - 1).unwrap();
        assert_eq!(allocator.snapshot().total, 0);
        assert_eq!(
            allocator.try_allocate(),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        );
    }

    #[test]
    fn failed_assign_keeps_reservation_recoverable() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let reservation = allocator.try_allocate().unwrap();
        allocator.begin_shutdown();
        assert_eq!(
            allocator.assign_inode(&reservation, 11),
            Err(SystemError::ENODEV)
        );
        allocator.cancel_reservation(&reservation).unwrap();
        allocator.finish_shutdown().unwrap();
    }

    #[test]
    fn generation_overflow_retires_slot_without_breaking_accounting() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        allocator.state.lock_irqsave().entries[0].generation = u64::MAX;
        assert_eq!(
            allocator.try_allocate(),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        );
        let snapshot = allocator.snapshot();
        assert_eq!(snapshot.retired, 1);
        assert_eq!(snapshot.free, 0);
        assert!(allocator.state.lock_irqsave().invariants_hold());
    }

    #[test]
    fn nonce_overflow_retires_slot_and_stale_token_is_rejected() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE * 2).unwrap();
        let stale = allocator.try_allocate().unwrap();
        allocator.cancel_reservation(&stale).unwrap();
        let current = allocator.try_allocate().unwrap();
        assert_eq!(
            allocator.cancel_reservation(&stale),
            Err(SystemError::EINVAL)
        );
        allocator.cancel_reservation(&current).unwrap();

        let mut state = allocator.state.lock_irqsave();
        let index = *state.free.front().unwrap();
        state.entries[index].transition_nonce = u64::MAX;
        drop(state);
        let reservation = allocator.try_allocate().unwrap();
        assert_ne!(reservation.window_offset(), index * DAX_RANGE_SIZE);
        allocator.cancel_reservation(&reservation).unwrap();
        assert_eq!(allocator.snapshot().retired, 1);
        assert!(allocator.state.lock_irqsave().invariants_hold());
    }

    #[test]
    fn reclaim_scan_is_bounded_and_rotates() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE * 3).unwrap();
        let _first = assign(&allocator, 1);
        let _second = assign(&allocator, 2);
        let _third = assign(&allocator, 3);

        let first = candidates(&allocator, 1);
        let second = candidates(&allocator, 1);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].inode(), 1);
        assert_eq!(second[0].inode(), 2);

        for candidate in first.into_iter().chain(second) {
            let reclaim = allocator.begin_reclaim(&candidate).unwrap();
            allocator.finish_reclaim(&reclaim).unwrap();
        }
        let third = candidates(&allocator, 1).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&third).unwrap();
        allocator.finish_reclaim(&reclaim).unwrap();
    }
}
