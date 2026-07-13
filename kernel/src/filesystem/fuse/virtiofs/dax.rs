use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::mem::size_of;

use log::error;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::fuse::{
        conn::{FuseConn, FuseDaxRequestOutcome},
        protocol::{
            fuse_pack_struct, FuseRemoveMappingIn, FuseRemoveMappingOne, FuseSetupMappingIn,
            FUSE_REMOVEMAPPING, FUSE_SETUPMAPPING, FUSE_SETUPMAPPING_FLAG_READ,
            FUSE_SETUPMAPPING_FLAG_WRITE,
        },
    },
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    mm::MemoryManagementArch,
};

pub(crate) const DAX_RANGE_SIZE: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaxMountMode {
    Never,
    Always,
    Inode,
}

#[derive(Debug)]
enum DaxRangeState {
    Free,
    Reserved { nonce: u64 },
    InodeOwned { owner: DaxMappingOwner, refs: u64 },
    Reclaiming { owner: DaxMappingOwner, nonce: u64 },
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DaxMappingOwner {
    nodeid: u64,
    incarnation: u64,
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
    owner: DaxMappingOwner,
}

#[derive(Debug, PartialEq, Eq)]
#[must_use = "a reclaim candidate must be revalidated before use"]
pub(crate) struct ReclaimCandidate {
    index: usize,
    generation: u64,
    owner: DaxMappingOwner,
}

#[derive(Debug, PartialEq, Eq)]
#[must_use = "an isolated DAX range must finish or cancel reclaim"]
pub(crate) struct ReclaimToken {
    index: usize,
    generation: u64,
    owner: DaxMappingOwner,
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

impl DaxMappingOwner {
    pub(in crate::filesystem::fuse) fn from_inode(
        nodeid: u64,
        incarnation: u64,
    ) -> Result<Self, SystemError> {
        if nodeid == 0 || incarnation == 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(Self {
            nodeid,
            incarnation,
        })
    }

    pub(crate) fn nodeid(self) -> u64 {
        self.nodeid
    }
}

impl OwnedToken {
    pub(crate) fn window_offset(&self) -> usize {
        self.index * DAX_RANGE_SIZE
    }

    pub(crate) fn len(&self) -> usize {
        DAX_RANGE_SIZE
    }

    pub(crate) fn owner(&self) -> DaxMappingOwner {
        self.owner
    }
}

impl ReclaimCandidate {
    pub(crate) fn window_offset(&self) -> usize {
        self.index * DAX_RANGE_SIZE
    }

    pub(crate) fn owner(&self) -> DaxMappingOwner {
        self.owner
    }
}

impl ReclaimToken {
    pub(crate) fn window_offset(&self) -> usize {
        self.index * DAX_RANGE_SIZE
    }

    pub(crate) fn len(&self) -> usize {
        DAX_RANGE_SIZE
    }

    pub(crate) fn owner(&self) -> DaxMappingOwner {
        self.owner
    }
}

fn validate_unique_reclaim_indexes(tokens: &[ReclaimToken]) -> Result<(), SystemError> {
    let mut indexes = Vec::new();
    indexes
        .try_reserve_exact(tokens.len())
        .map_err(|_| SystemError::ENOMEM)?;
    indexes.extend(tokens.iter().map(|token| token.index));
    indexes.sort_unstable();
    if indexes.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

impl DaxRangeAllocatorState {
    fn can_make_progress(&self) -> bool {
        !self.free.is_empty()
            || self
                .entries
                .iter()
                .any(|entry| matches!(entry.state, DaxRangeState::InodeOwned { refs: 1, .. }))
    }

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
            } else if state.can_make_progress() {
                Some(Ok(()))
            } else {
                None
            }
        })?
    }

    pub(crate) fn assign_owner(
        &self,
        token: &AllocationToken,
        owner: DaxMappingOwner,
    ) -> Result<OwnedToken, SystemError> {
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
        entry.state = DaxRangeState::InodeOwned { owner, refs: 1 };
        let owned = OwnedToken {
            index: token.index,
            generation: token.generation,
            owner,
        };
        drop(state);
        self.wait.wakeup(None);
        Ok(owned)
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

    pub(crate) fn retire_reservation(&self, token: &AllocationToken) -> Result<(), SystemError> {
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
        entry.state = DaxRangeState::Retired;
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
        let DaxRangeState::InodeOwned { owner, refs } = &mut entry.state else {
            return Err(SystemError::EINVAL);
        };
        if *owner != token.owner {
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
        let DaxRangeState::InodeOwned { owner, refs } = &mut entry.state else {
            return Err(SystemError::EINVAL);
        };
        if *owner != token.owner || *refs <= 1 {
            return Err(SystemError::EINVAL);
        }
        *refs -= 1;
        let became_reclaimable = *refs == 1;
        drop(state);
        if became_reclaimable {
            self.wait.wakeup(None);
        }
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
            if let DaxRangeState::InodeOwned { owner, refs: 1 } = entry.state {
                candidates.push(ReclaimCandidate {
                    index,
                    generation: entry.generation,
                    owner,
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
            || !matches!(entry.state, DaxRangeState::InodeOwned { owner, refs: 1 } if owner == candidate.owner)
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let nonce = entry
            .transition_nonce
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        entry.transition_nonce = nonce;
        entry.state = DaxRangeState::Reclaiming {
            owner: candidate.owner,
            nonce,
        };
        Ok(ReclaimToken {
            index: candidate.index,
            generation: candidate.generation,
            owner: candidate.owner,
            nonce,
        })
    }

    pub(crate) fn finish_reclaim(&self, token: &ReclaimToken) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        Self::validate_reclaim_token(&state, token)?;
        state.entries[token.index].state = DaxRangeState::Free;
        state.free.push_back(token.index);
        drop(state);
        self.wait.wakeup(None);
        Ok(())
    }

    pub(crate) fn retire_reclaim(&self, token: &ReclaimToken) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        Self::validate_reclaim_token(&state, token)?;
        state.entries[token.index].state = DaxRangeState::Retired;
        Ok(())
    }

    pub(crate) fn cancel_reclaim(&self, token: &ReclaimToken) -> Result<OwnedToken, SystemError> {
        let mut state = self.state.lock_irqsave();
        Self::validate_reclaim_token(&state, token)?;
        state.entries[token.index].state = DaxRangeState::InodeOwned {
            owner: token.owner,
            refs: 1,
        };
        let owned = OwnedToken {
            index: token.index,
            generation: token.generation,
            owner: token.owner,
        };
        drop(state);
        self.wait.wakeup(None);
        Ok(owned)
    }

    fn validate_reclaim_token(
        state: &DaxRangeAllocatorState,
        token: &ReclaimToken,
    ) -> Result<(), SystemError> {
        let entry = state.entries.get(token.index).ok_or(SystemError::EINVAL)?;
        if entry.generation != token.generation
            || !matches!(entry.state, DaxRangeState::Reclaiming { owner, nonce } if owner == token.owner && nonce == token.nonce)
        {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn validate_reclaim_batch(
        &self,
        owner: DaxMappingOwner,
        tokens: &[ReclaimToken],
    ) -> Result<(), SystemError> {
        let state = self.state.lock_irqsave();
        for token in tokens {
            if token.owner != owner {
                return Err(SystemError::EINVAL);
            }
            Self::validate_reclaim_token(&state, token)?;
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

    /// Finish local accounting after the FUSE connection is confirmed dead.
    ///
    /// The cache window belongs to this connection and is never handed to a
    /// replacement session, so no daemon can observe aliases after this point.
    pub(crate) fn disconnect_cleanup(&self) -> usize {
        let mut state = self.state.lock_irqsave();
        state.shutdown = true;
        state.free.clear();
        let mut cleaned = 0;
        for entry in &mut state.entries {
            if !matches!(entry.state, DaxRangeState::Free | DaxRangeState::Retired) {
                cleaned += 1;
            }
            entry.state = DaxRangeState::Retired;
        }
        drop(state);
        self.wait.wakeup_all(None);
        cleaned
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

impl FuseConn {
    const REMOVE_MAPPING_MAX_ENTRIES: usize = MMArch::PAGE_SIZE / size_of::<FuseRemoveMappingOne>();

    fn dax_allocator_clone(&self) -> Result<Arc<DaxRangeAllocator>, SystemError> {
        self.dax_allocator()
            .cloned()
            .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn validate_mapping_range(
        &self,
        window_offset: usize,
        file_offset: u64,
        len: usize,
    ) -> Result<(), SystemError> {
        let alignment_shift = self
            .dax_map_alignment()
            .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
        let alignment = 1u64
            .checked_shl(u32::from(alignment_shift))
            .ok_or(SystemError::EINVAL)?;
        let len_u64 = u64::try_from(len).map_err(|_| SystemError::EOVERFLOW)?;
        let window_offset_u64 = u64::try_from(window_offset).map_err(|_| SystemError::EOVERFLOW)?;
        file_offset
            .checked_add(len_u64)
            .ok_or(SystemError::EOVERFLOW)?;
        window_offset_u64
            .checked_add(len_u64)
            .ok_or(SystemError::EOVERFLOW)?;
        if len == 0
            || window_offset % DAX_RANGE_SIZE != 0
            || len % DAX_RANGE_SIZE != 0
            || file_offset % DAX_RANGE_SIZE as u64 != 0
            || file_offset % alignment != 0
            || window_offset_u64 % alignment != 0
            || len_u64 % alignment != 0
        {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn send_setup_mapping(
        &self,
        owner: DaxMappingOwner,
        fh: u64,
        file_offset: u64,
        window_offset: usize,
        writable: bool,
    ) -> FuseDaxRequestOutcome {
        if let Err(error) = self.validate_mapping_range(window_offset, file_offset, DAX_RANGE_SIZE)
        {
            return FuseDaxRequestOutcome::NeverSubmitted(error);
        }
        let input = FuseSetupMappingIn {
            fh,
            foffset: file_offset,
            len: DAX_RANGE_SIZE as u64,
            flags: FUSE_SETUPMAPPING_FLAG_READ
                | if writable {
                    FUSE_SETUPMAPPING_FLAG_WRITE
                } else {
                    0
                },
            moffset: window_offset as u64,
        };
        self.request_dax_mapping(FUSE_SETUPMAPPING, owner.nodeid(), fuse_pack_struct(&input))
    }

    fn apply_setup_outcome(
        allocator: &DaxRangeAllocator,
        reservation: &AllocationToken,
        owner: DaxMappingOwner,
        outcome: FuseDaxRequestOutcome,
    ) -> Result<OwnedToken, SystemError> {
        match outcome {
            FuseDaxRequestOutcome::Success => match allocator.assign_owner(reservation, owner) {
                Ok(owned) => Ok(owned),
                Err(error) => {
                    let _ = allocator.retire_reservation(reservation);
                    Err(error)
                }
            },
            FuseDaxRequestOutcome::NeverSubmitted(error)
            | FuseDaxRequestOutcome::DaemonError(error) => {
                let _ = allocator.cancel_reservation(reservation);
                Err(error)
            }
            FuseDaxRequestOutcome::OutcomeUnknown(error)
            | FuseDaxRequestOutcome::Disconnected(error) => {
                let _ = allocator.retire_reservation(reservation);
                Err(error)
            }
        }
    }

    pub(crate) fn setup_dax_mapping(
        &self,
        owner: DaxMappingOwner,
        fh: u64,
        file_offset: u64,
        writable: bool,
    ) -> Result<OwnedToken, SystemError> {
        let allocator = self.dax_allocator_clone()?;
        let reservation = allocator.try_allocate()?;
        let result = self.send_setup_mapping(
            owner,
            fh,
            file_offset,
            reservation.window_offset(),
            writable,
        );
        Self::apply_setup_outcome(&allocator, &reservation, owner, result)
    }

    /// Send SETUPMAPPING for an existing range (used by #2080 for access upgrades).
    pub(crate) fn setup_existing_dax_mapping(
        &self,
        owner: DaxMappingOwner,
        token: &OwnedToken,
        fh: u64,
        file_offset: u64,
        writable: bool,
    ) -> Result<(), SystemError> {
        if token.owner() != owner {
            return Err(SystemError::EINVAL);
        }
        let allocator = self.dax_allocator_clone()?;
        allocator.get(token)?;
        let outcome =
            self.send_setup_mapping(owner, fh, file_offset, token.window_offset(), writable);
        let put_result = allocator.put(token);
        match outcome {
            FuseDaxRequestOutcome::Success => {
                put_result?;
                Ok(())
            }
            FuseDaxRequestOutcome::NeverSubmitted(error)
            | FuseDaxRequestOutcome::DaemonError(error)
            | FuseDaxRequestOutcome::OutcomeUnknown(error)
            | FuseDaxRequestOutcome::Disconnected(error) => match put_result {
                Ok(()) => Err(error),
                // disconnect_cleanup invalidates every token after closing the
                // allocator. Preserve the request's real terminal errno rather
                // than replacing it with the expected local EINVAL from put().
                Err(SystemError::EINVAL) if !self.is_connected() => Err(error),
                Err(put_error) => Err(put_error),
            },
        }
    }

    pub(crate) fn remove_dax_mappings(
        &self,
        owner: DaxMappingOwner,
        tokens: &[ReclaimToken],
    ) -> Result<(), SystemError> {
        self.remove_dax_mappings_with(owner, tokens, |payload| {
            self.request_dax_mapping(FUSE_REMOVEMAPPING, owner.nodeid(), payload)
        })
    }

    fn remove_dax_mappings_with<F>(
        &self,
        owner: DaxMappingOwner,
        tokens: &[ReclaimToken],
        mut request: F,
    ) -> Result<(), SystemError>
    where
        F: FnMut(&[u8]) -> FuseDaxRequestOutcome,
    {
        if tokens.is_empty() {
            return Ok(());
        }
        let allocator = self.dax_allocator_clone()?;
        let rollback_all = |tokens: &[ReclaimToken]| {
            for token in tokens {
                let _ = allocator.cancel_reclaim(token);
            }
        };

        if let Err(error) = validate_unique_reclaim_indexes(tokens) {
            rollback_all(tokens);
            return Err(error);
        }
        if let Err(error) = allocator.validate_reclaim_batch(owner, tokens) {
            rollback_all(tokens);
            return Err(error);
        }

        let max_entries = core::cmp::min(Self::REMOVE_MAPPING_MAX_ENTRIES, tokens.len());
        let max_payload_len = match max_entries
            .checked_mul(size_of::<FuseRemoveMappingOne>())
            .and_then(|entries| size_of::<FuseRemoveMappingIn>().checked_add(entries))
        {
            Some(len) => len,
            None => {
                rollback_all(tokens);
                return Err(SystemError::EOVERFLOW);
            }
        };
        let mut payload = Vec::new();
        if payload.try_reserve_exact(max_payload_len).is_err() {
            rollback_all(tokens);
            return Err(SystemError::ENOMEM);
        }

        let mut start = 0;
        while start < tokens.len() {
            let end = core::cmp::min(
                start.saturating_add(Self::REMOVE_MAPPING_MAX_ENTRIES),
                tokens.len(),
            );
            let batch = &tokens[start..end];
            payload.clear();
            let header = FuseRemoveMappingIn {
                // A batch is capped at PAGE_SIZE / 16, far below u32::MAX.
                count: batch.len() as u32,
            };
            payload.extend_from_slice(fuse_pack_struct(&header));
            for token in batch {
                let entry = FuseRemoveMappingOne {
                    moffset: token.window_offset() as u64,
                    len: token.len() as u64,
                };
                payload.extend_from_slice(fuse_pack_struct(&entry));
            }

            match request(&payload) {
                FuseDaxRequestOutcome::Success => {
                    for token in batch {
                        allocator.finish_reclaim(token)?;
                    }
                }
                FuseDaxRequestOutcome::NeverSubmitted(error) => {
                    rollback_all(&tokens[start..]);
                    return Err(error);
                }
                FuseDaxRequestOutcome::DaemonError(error)
                | FuseDaxRequestOutcome::OutcomeUnknown(error) => {
                    for token in batch {
                        let _ = allocator.retire_reclaim(token);
                    }
                    rollback_all(&tokens[end..]);
                    return Err(error);
                }
                FuseDaxRequestOutcome::Disconnected(error) => {
                    return Err(error);
                }
            }
            start = end;
        }
        Ok(())
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

    fn owner(nodeid: u64) -> DaxMappingOwner {
        DaxMappingOwner::from_inode(nodeid, nodeid).unwrap()
    }

    fn assign(allocator: &DaxRangeAllocator, nodeid: u64) -> OwnedToken {
        let reservation = allocator.try_allocate().unwrap();
        allocator.assign_owner(&reservation, owner(nodeid)).unwrap()
    }

    fn reclaim_tokens(
        allocator: &DaxRangeAllocator,
        mapping_owner: DaxMappingOwner,
        count: usize,
    ) -> Vec<ReclaimToken> {
        for _ in 0..count {
            let reservation = allocator.try_allocate().unwrap();
            allocator.assign_owner(&reservation, mapping_owner).unwrap();
        }
        let mut candidates = Vec::with_capacity(count);
        allocator
            .reclaim_candidates(&mut candidates, count)
            .unwrap();
        assert_eq!(candidates.len(), count);
        candidates
            .iter()
            .map(|candidate| allocator.begin_reclaim(candidate).unwrap())
            .collect()
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
    fn mapping_range_validation_enforces_chunk_alignment_and_overflow() {
        let conn = FuseConn::new_for_virtiofs_with_dax(
            8192,
            8192,
            Some(DAX_RANGE_SIZE),
            DaxMountMode::Always,
        );
        assert_eq!(conn.validate_mapping_range(0, 0, DAX_RANGE_SIZE), Ok(()));
        assert_eq!(
            conn.validate_mapping_range(0, 4096, DAX_RANGE_SIZE),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            conn.validate_mapping_range(4096, 0, DAX_RANGE_SIZE),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            conn.validate_mapping_range(0, u64::MAX - 4095, DAX_RANGE_SIZE),
            Err(SystemError::EOVERFLOW)
        );
    }

    #[test]
    fn setup_outcome_publishes_rolls_back_or_retires_by_provenance() {
        let mapping_owner = owner(7);

        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let reservation = allocator.try_allocate().unwrap();
        let owned = FuseConn::apply_setup_outcome(
            &allocator,
            &reservation,
            mapping_owner,
            FuseDaxRequestOutcome::Success,
        )
        .unwrap();
        assert_eq!(allocator.snapshot().inode_owned, 1);
        assert_eq!(owned.owner(), mapping_owner);
        allocator.disconnect_cleanup();

        for outcome in [
            FuseDaxRequestOutcome::NeverSubmitted(SystemError::ENOMEM),
            FuseDaxRequestOutcome::DaemonError(SystemError::EIO),
        ] {
            let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
            let reservation = allocator.try_allocate().unwrap();
            assert!(FuseConn::apply_setup_outcome(
                &allocator,
                &reservation,
                mapping_owner,
                outcome,
            )
            .is_err());
            let snapshot = allocator.snapshot();
            assert_eq!(snapshot.free, 1);
            assert_eq!(snapshot.retired, 0);
        }

        for outcome in [
            FuseDaxRequestOutcome::OutcomeUnknown(SystemError::EIO),
            FuseDaxRequestOutcome::Disconnected(SystemError::ENOTCONN),
        ] {
            let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
            let reservation = allocator.try_allocate().unwrap();
            assert!(FuseConn::apply_setup_outcome(
                &allocator,
                &reservation,
                mapping_owner,
                outcome,
            )
            .is_err());
            let snapshot = allocator.snapshot();
            assert_eq!(snapshot.free, 0);
            assert_eq!(snapshot.retired, 1);
        }
    }

    #[test]
    fn remove_success_frees_only_after_reply() {
        let count = 2;
        let conn = FuseConn::new_for_virtiofs_with_dax(
            8192,
            8192,
            Some(DAX_RANGE_SIZE * count),
            DaxMountMode::Always,
        );
        let allocator = conn.dax_allocator().unwrap();
        let mapping_owner = owner(11);
        let tokens = reclaim_tokens(allocator, mapping_owner, count);
        conn.remove_dax_mappings_with(mapping_owner, &tokens, |_| {
            assert_eq!(allocator.snapshot().reclaiming, count);
            FuseDaxRequestOutcome::Success
        })
        .unwrap();
        assert_eq!(allocator.snapshot().free, count);
    }

    #[test]
    fn remove_failure_restores_unsent_and_isolates_submitted_batch() {
        let batch = FuseConn::REMOVE_MAPPING_MAX_ENTRIES;
        let count = batch + 1;
        let conn = FuseConn::new_for_virtiofs_with_dax(
            8192,
            8192,
            Some(DAX_RANGE_SIZE * count),
            DaxMountMode::Always,
        );
        let allocator = conn.dax_allocator().unwrap();
        let mapping_owner = owner(13);
        let tokens = reclaim_tokens(allocator, mapping_owner, count);
        let mut calls = 0;
        assert_eq!(
            conn.remove_dax_mappings_with(mapping_owner, &tokens, |_| {
                calls += 1;
                if calls == 1 {
                    FuseDaxRequestOutcome::Success
                } else {
                    FuseDaxRequestOutcome::OutcomeUnknown(SystemError::EIO)
                }
            }),
            Err(SystemError::EIO)
        );
        assert_eq!(calls, 2);
        let snapshot = allocator.snapshot();
        assert_eq!(snapshot.free, batch);
        assert_eq!(snapshot.retired, 1);
        assert_eq!(snapshot.inode_owned, 0);
    }

    #[test]
    fn remove_never_submitted_restores_entire_batch() {
        let conn = FuseConn::new_for_virtiofs_with_dax(
            8192,
            8192,
            Some(DAX_RANGE_SIZE * 2),
            DaxMountMode::Always,
        );
        let allocator = conn.dax_allocator().unwrap();
        let mapping_owner = owner(17);
        let tokens = reclaim_tokens(allocator, mapping_owner, 2);
        assert_eq!(
            conn.remove_dax_mappings_with(mapping_owner, &tokens, |_| {
                FuseDaxRequestOutcome::NeverSubmitted(SystemError::ENOMEM)
            }),
            Err(SystemError::ENOMEM)
        );
        let snapshot = allocator.snapshot();
        assert_eq!(snapshot.inode_owned, 2);
        assert_eq!(snapshot.retired, 0);
        allocator.disconnect_cleanup();
    }

    #[test]
    fn remove_daemon_error_isolates_and_disconnect_cleans_up() {
        let conn = FuseConn::new_for_virtiofs_with_dax(
            8192,
            8192,
            Some(DAX_RANGE_SIZE),
            DaxMountMode::Always,
        );
        let allocator = conn.dax_allocator().unwrap();
        let mapping_owner = owner(19);
        let tokens = reclaim_tokens(allocator, mapping_owner, 1);
        assert_eq!(
            conn.remove_dax_mappings_with(mapping_owner, &tokens, |_| {
                FuseDaxRequestOutcome::DaemonError(SystemError::EBUSY)
            }),
            Err(SystemError::EBUSY)
        );
        assert_eq!(allocator.snapshot().retired, 1);

        let disconnected = FuseConn::new_for_virtiofs_with_dax(
            8192,
            8192,
            Some(DAX_RANGE_SIZE),
            DaxMountMode::Always,
        );
        let disconnected_allocator = disconnected.dax_allocator().unwrap();
        let tokens = reclaim_tokens(disconnected_allocator, mapping_owner, 1);
        assert_eq!(
            disconnected.remove_dax_mappings_with(mapping_owner, &tokens, |_| {
                disconnected.abort();
                FuseDaxRequestOutcome::Disconnected(SystemError::ENOTCONN)
            }),
            Err(SystemError::ENOTCONN)
        );
        let snapshot = disconnected_allocator.snapshot();
        assert!(snapshot.shutdown);
        assert_eq!(snapshot.retired, 1);
    }

    #[test]
    fn references_protect_mapping_from_reclaim() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let owned = assign(&allocator, 7);
        assert_eq!(owned.owner(), owner(7));
        assert_eq!(owned.window_offset(), 0);
        assert_eq!(owned.len(), DAX_RANGE_SIZE);
        assert!(allocator.state.lock_irqsave().can_make_progress());
        allocator.get(&owned).unwrap();
        assert!(!allocator.state.lock_irqsave().can_make_progress());
        assert!(candidates(&allocator, 10).is_empty());
        allocator.put(&owned).unwrap();
        assert!(allocator.state.lock_irqsave().can_make_progress());
        let candidate = candidates(&allocator, 10).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&candidate).unwrap();
        allocator.finish_reclaim(&reclaim).unwrap();
        assert_eq!(allocator.snapshot().free, 1);
    }

    #[test]
    fn exhausted_wait_can_progress_from_reclaimable_mapping() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let reservation = allocator.try_allocate().unwrap();
        assert!(!allocator.state.lock_irqsave().can_make_progress());

        let owned = allocator.assign_owner(&reservation, owner(12)).unwrap();
        assert!(allocator.state.lock_irqsave().can_make_progress());
        allocator.get(&owned).unwrap();
        assert!(!allocator.state.lock_irqsave().can_make_progress());
        allocator.put(&owned).unwrap();
        assert!(allocator.state.lock_irqsave().can_make_progress());

        let candidate = candidates(&allocator, 1).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&candidate).unwrap();
        assert!(!allocator.state.lock_irqsave().can_make_progress());
        let owned = allocator.cancel_reclaim(&reclaim).unwrap();
        assert!(allocator.state.lock_irqsave().can_make_progress());

        let candidate = candidates(&allocator, 1).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&candidate).unwrap();
        drop(owned);
        allocator.finish_reclaim(&reclaim).unwrap();
    }

    #[test]
    fn reclaim_revalidates_and_can_be_cancelled() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let owned = assign(&allocator, 9);
        let stale = candidates(&allocator, 1).pop().unwrap();
        assert_eq!(stale.window_offset(), 0);
        assert_eq!(stale.owner(), owner(9));
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
    fn uncertain_setup_is_retired_until_disconnect() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let reservation = allocator.try_allocate().unwrap();
        allocator.retire_reservation(&reservation).unwrap();
        assert_eq!(allocator.snapshot().retired, 1);
        assert_eq!(
            allocator.try_allocate(),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        );
        assert_eq!(allocator.disconnect_cleanup(), 0);
        assert!(allocator.state.lock_irqsave().invariants_hold());
    }

    #[test]
    fn disconnect_cleanup_invalidates_every_live_state() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE * 3).unwrap();
        let reserved = allocator.try_allocate().unwrap();
        let owned = assign(&allocator, 2);
        let reclaim_owned = assign(&allocator, 3);
        let candidate = candidates(&allocator, 3)
            .into_iter()
            .find(|candidate| candidate.owner() == reclaim_owned.owner())
            .unwrap();
        let reclaim = allocator.begin_reclaim(&candidate).unwrap();

        assert_eq!(allocator.disconnect_cleanup(), 3);
        let snapshot = allocator.snapshot();
        assert!(snapshot.shutdown);
        assert_eq!(snapshot.retired, 3);
        assert_eq!(
            snapshot.free + snapshot.reserved + snapshot.inode_owned + snapshot.reclaiming,
            0
        );
        assert_eq!(
            allocator.cancel_reservation(&reserved),
            Err(SystemError::EINVAL)
        );
        assert_eq!(allocator.get(&owned), Err(SystemError::EINVAL));
        assert_eq!(allocator.finish_reclaim(&reclaim), Err(SystemError::EINVAL));
    }

    #[test]
    fn reclaim_batch_rejects_cross_generation_owner_and_duplicates() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE * 2).unwrap();
        let first = assign(&allocator, 9);
        let second_reservation = allocator.try_allocate().unwrap();
        let second_owner = DaxMappingOwner::from_inode(9, 2).unwrap();
        let second = allocator
            .assign_owner(&second_reservation, second_owner)
            .unwrap();
        let mut all = candidates(&allocator, 2);
        let first_candidate = all
            .iter()
            .position(|candidate| candidate.owner() == first.owner())
            .map(|position| all.swap_remove(position))
            .unwrap();
        let second_candidate = all.pop().unwrap();
        let first_reclaim = allocator.begin_reclaim(&first_candidate).unwrap();
        let second_reclaim = allocator.begin_reclaim(&second_candidate).unwrap();
        assert_eq!(
            allocator.validate_reclaim_batch(first.owner(), &[first_reclaim, second_reclaim]),
            Err(SystemError::EINVAL)
        );
        allocator.disconnect_cleanup();
    }

    #[test]
    fn reclaim_batch_rejects_duplicate_range() {
        let allocator = DaxRangeAllocator::new(DAX_RANGE_SIZE).unwrap();
        let owned = assign(&allocator, 7);
        let candidate = candidates(&allocator, 1).pop().unwrap();
        let token = allocator.begin_reclaim(&candidate).unwrap();
        let duplicate = ReclaimToken {
            index: token.index,
            generation: token.generation,
            owner: token.owner,
            nonce: token.nonce,
        };
        assert_eq!(
            validate_unique_reclaim_indexes(&[token, duplicate]),
            Err(SystemError::EINVAL)
        );
        allocator.disconnect_cleanup();
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
            allocator.assign_owner(&reservation, owner(11)),
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
        assert_eq!(first[0].owner(), owner(1));
        assert_eq!(second[0].owner(), owner(2));

        for candidate in first.into_iter().chain(second) {
            let reclaim = allocator.begin_reclaim(&candidate).unwrap();
            allocator.finish_reclaim(&reclaim).unwrap();
        }
        let third = candidates(&allocator, 1).pop().unwrap();
        let reclaim = allocator.begin_reclaim(&third).unwrap();
        allocator.finish_reclaim(&reclaim).unwrap();
    }
}
