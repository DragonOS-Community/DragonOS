//! Bounded logical metadata publication, separate from journal durability.
//!
//! There is one operation writer, one Running image set and at most one
//! immutable Frozen set. The mount owns scheduling and waiting; this module
//! never sleeps and never invokes callbacks while holding its state lock.
//! It must only be selected after *all* runtime metadata access uses this view.

use super::journal_transaction::{
    CachePublisher, CommitError, JournalContext, JournalTransactionCore, MetadataBlockSource,
    StagedBlock, Transaction, TransactionCoreRef,
};
use super::MetadataMutationWaker;
use crate::constants::BLOCK_SIZE;
use crate::ext4_defs::{Block, BlockDevice};
use crate::prelude::*;

/// Logical sequence numbers are checked, not wrapping JBD2 transaction IDs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchProgress {
    pub accepted: u64,
    pub durable: u64,
    /// Equality token for wait/recheck and multi-block optimistic readers.
    pub generation: u64,
    pub running_blocks: usize,
    pub frozen_blocks: usize,
    pub seal_requested: bool,
    pub active_operation: bool,
    pub publishing: bool,
    pub failed: bool,
    pub closed: bool,
    pub running_retirements: usize,
    pub frozen_retirements: usize,
    pub running_operations: usize,
    pub frozen_operations: usize,
}

/// Half-open ranges keep large extent releases bounded by metadata work,
/// rather than allocating one retirement node for every freed data block.
#[derive(Clone, Copy)]
pub(super) struct RetiredRange {
    inode: bool,
    start: u64,
    end: u64,
}

impl RetiredRange {
    pub(super) fn blocks(start: PBlockId, count: u64) -> Result<Self> {
        let end = start
            .checked_add(count)
            .filter(|end| *end > start)
            .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
        Ok(Self {
            inode: false,
            start,
            end,
        })
    }

    pub(super) fn inode(id: InodeId) -> Self {
        Self {
            inode: true,
            start: id as u64,
            end: id as u64 + 1,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        self.inode == other.inode && self.start < other.end && other.start < self.end
    }

    pub(super) fn block_bounds(self) -> Option<(PBlockId, PBlockId)> {
        (!self.inode).then_some((self.start, self.end))
    }
}

/// Called before any allocator mutation. Capacity allocation is only needed
/// in the private operation; batch vectors were fully reserved at mount.
pub(super) fn retire(
    ranges: &mut Vec<RetiredRange>,
    limit: usize,
    mut new: RetiredRange,
) -> Result<()> {
    // Find the transitive union before modifying the list, so every error
    // still leaves the operation's existing retirement ownership intact.
    loop {
        let previous = (new.start, new.end);
        for old in ranges.iter() {
            if old.inode == new.inode && old.start <= new.end && new.start <= old.end {
                new.start = new.start.min(old.start);
                new.end = new.end.max(old.end);
            }
        }
        if previous == (new.start, new.end) {
            break;
        }
    }
    let merged = ranges
        .iter()
        .filter(|old| old.inode == new.inode && old.start <= new.end && new.start <= old.end)
        .count();
    if ranges.len() - merged >= limit {
        return Err(Ext4Error::new(ErrCode::E2BIG));
    }
    if merged == 0 {
        ranges
            .try_reserve(1)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
    }
    ranges.retain(|old| !(old.inode == new.inode && old.start <= new.end && new.start <= old.end));
    ranges.push(new);
    Ok(())
}

pub(super) fn reusable(ranges: &[RetiredRange], query: RetiredRange) -> bool {
    !ranges.iter().any(|retired| retired.overlaps(query))
}

pub(super) fn reuse_restart(ranges: &[RetiredRange], query: RetiredRange) -> Option<u64> {
    ranges
        .iter()
        .filter(|retired| retired.overlaps(query))
        .map(|retired| retired.end)
        .max()
}

struct Frozen {
    blocks: Vec<StagedBlock>,
    retired: Vec<RetiredRange>,
    operations: usize,
}

struct State {
    running: Vec<StagedBlock>,
    retired: Vec<RetiredRange>,
    spare: Option<Arc<Frozen>>,
    frozen: Option<Arc<Frozen>>,
    accepted: u64,
    durable: u64,
    generation: u64,
    reserved: usize,
    operations: usize,
    active: bool,
    publishing: bool,
    committing: bool,
    seal_requested: bool,
    failed: bool,
    closed: bool,
}

pub struct JournalBatchCore {
    journal: JournalTransactionCore,
    capacity: usize,
    state: spin::Mutex<State>,
    waker: spin::Once<Arc<dyn MetadataMutationWaker>>,
}

impl JournalBatchCore {
    /// `block_budget` bounds each live image set and each private operation.
    /// The effective limit is additionally constrained by descriptor/tag space
    /// and the journal ring, using the same calculation as the disk encoder.
    pub fn new(context: JournalContext, block_budget: usize) -> Result<Self> {
        JournalTransactionCore::new(context)?.into_batch(block_budget)
    }

    pub(super) fn from_journal(
        journal: JournalTransactionCore,
        block_budget: usize,
    ) -> Result<Self> {
        if block_budget == 0 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let mut low = 0;
        let mut high = block_budget;
        while low < high {
            let mid = low + (high - low) / 2 + 1;
            if journal.credits_fit(mid)? {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        if low == 0 {
            return Err(Ext4Error::new(ErrCode::E2BIG));
        }
        let mut running = Vec::new();
        let mut spare = Vec::new();
        let mut retired = Vec::new();
        let mut spare_retired = Vec::new();
        running
            .try_reserve_exact(low)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        spare
            .try_reserve_exact(low)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        retired
            .try_reserve_exact(low)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        spare_retired
            .try_reserve_exact(low)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        Ok(Self {
            journal,
            capacity: low,
            state: spin::Mutex::new(State {
                running,
                retired,
                spare: Some(Arc::new(Frozen {
                    blocks: spare,
                    retired: spare_retired,
                    operations: 0,
                })),
                frozen: None,
                accepted: 0,
                durable: 0,
                generation: 0,
                reserved: 0,
                operations: 0,
                active: false,
                publishing: false,
                committing: false,
                seal_requested: false,
                failed: false,
                closed: false,
            }),
            waker: spin::Once::new(),
        })
    }

    pub fn install_waker(&self, waker: Arc<dyn MetadataMutationWaker>) {
        self.waker.call_once(|| waker);
    }

    fn wake(&self) {
        if let Some(waker) = self.waker.get() {
            waker.wake_all();
        }
    }

    pub fn is_poisoned(&self) -> bool {
        self.state.lock().failed
    }

    pub fn credits_fit(&self, credits: usize) -> Result<bool> {
        if credits == 0 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if self.is_poisoned() {
            return Err(Ext4Error::new(ErrCode::EROFS));
        }
        Ok(credits <= self.capacity)
    }

    pub fn can_shutdown(&self) -> bool {
        let state = self.state.lock();
        !state.active
            && !state.committing
            && state.running.is_empty()
            && state.frozen.is_none()
            && !state.failed
    }

    pub fn progress(&self) -> BatchProgress {
        let state = self.state.lock();
        BatchProgress {
            accepted: state.accepted,
            durable: state.durable,
            generation: state.generation,
            running_blocks: state.running.len(),
            frozen_blocks: state.frozen.as_ref().map_or(0, |batch| batch.blocks.len()),
            seal_requested: state.seal_requested,
            active_operation: state.active,
            publishing: state.publishing,
            failed: state.failed,
            closed: state.closed,
            running_retirements: state.retired.len(),
            frozen_retirements: state.frozen.as_ref().map_or(0, |batch| batch.retired.len()),
            running_operations: state.operations,
            frozen_operations: state.frozen.as_ref().map_or(0, |batch| batch.operations),
        }
    }

    pub fn owns_block_range(&self, start: PBlockId, end: PBlockId) -> bool {
        self.journal.owns_block_range(start, end)
    }

    pub(super) fn validate_home(&self, home: PBlockId) -> Result<()> {
        self.journal.validate_home(home)
    }

    /// Reserve worst-case unique images before entering an operation. EAGAIN
    /// means release upper locks and wait/revalidate, never retry in a spin.
    pub fn start(&self, credits: usize) -> Result<Transaction<'_>> {
        if credits == 0 || credits > self.capacity {
            return Err(Ext4Error::new(if credits == 0 {
                ErrCode::EINVAL
            } else {
                ErrCode::E2BIG
            }));
        }
        let mut state = self.state.lock();
        if state.failed || state.closed {
            return Err(Ext4Error::new(ErrCode::EROFS));
        }
        if state.active || state.seal_requested {
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }
        if credits > self.capacity - state.running.len()
            || credits > self.capacity - state.retired.len()
            || state.operations == self.capacity
        {
            state.seal_requested = true;
            state.generation = state.generation.wrapping_add(1);
            drop(state);
            self.wake();
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }
        state.active = true;
        state.reserved = credits;
        drop(state);
        Ok(Transaction::new(
            TransactionCoreRef::Batch(self),
            credits,
            false,
        ))
    }

    pub(super) fn release_operation(&self) {
        let mut state = self.state.lock();
        state.active = false;
        state.reserved = 0;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.wake();
    }

    /// The caller owns the operation token and its existing inode/namespace
    /// locks. Its publisher is infallible. The publishing flag and generation
    /// bracket cache updates performed outside the engine spinlock; readers
    /// of upper caches must participate in the same view validation protocol.
    pub(super) fn publish(
        &self,
        staged: &mut BTreeMap<PBlockId, StagedBlock>,
        retired: &mut Vec<RetiredRange>,
        publisher: &dyn CachePublisher,
    ) -> Result<u64> {
        // Deallocate map nodes and replaced images outside the engine lock.
        // Reserve both owner arrays before any cache becomes visible.
        let mut incoming = Vec::new();
        let mut replaced = Vec::new();
        incoming
            .try_reserve_exact(staged.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        replaced
            .try_reserve_exact(staged.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        let sequence = {
            let mut state = self.state.lock();
            if state.failed || state.closed {
                return Err(Ext4Error::new(ErrCode::EROFS));
            }
            if !state.active || staged.len() > state.reserved || retired.len() > state.reserved {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            if staged.is_empty() {
                if !retired.is_empty() {
                    return Err(Ext4Error::new(ErrCode::EINVAL));
                }
                return Ok(state.accepted);
            }
            let next = state
                .accepted
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::ERANGE))?;
            state.publishing = true;
            state.generation = state.generation.wrapping_add(1);
            next
        };
        publisher.publish_accepted(staged, sequence, retired);
        incoming.extend(core::mem::take(staged).into_values());
        // Capacity and all validation were settled before the first visible
        // cache change. Sorted Vec insertion moves only small block owners,
        // never allocates nodes, and binary search serves metadata reads.
        let mut state = self.state.lock();
        for block in incoming.drain(..) {
            let home = block.home();
            match state.running.binary_search_by_key(&home, StagedBlock::home) {
                Ok(index) => replaced.push(core::mem::replace(&mut state.running[index], block)),
                Err(index) => state.running.insert(index, block),
            }
        }
        for range in retired.drain(..) {
            // Admission reserved worst-case independent records, so this
            // merge cannot allocate or fail after logical publication.
            retire(&mut state.retired, self.capacity, range).expect("reserved retirement capacity");
        }
        state.accepted = sequence;
        state.operations += 1;
        state.publishing = false;
        state.generation = state.generation.wrapping_add(1);
        if state.running.len() == self.capacity
            || state.retired.len() == self.capacity
            || state.operations == self.capacity
        {
            state.seal_requested = true;
        }
        drop(state);
        self.wake();
        Ok(sequence)
    }

    /// Read from the latest accepted view. Allocate the destination before
    /// locking; a cold read is revalidated after I/O even when an intervening
    /// checkpoint removed the last overlay version of this home.
    pub(super) fn read_metadata<S: MetadataBlockSource + ?Sized>(
        &self,
        source: &S,
        home: PBlockId,
    ) -> Result<Block> {
        self.validate_home(home)?;
        let mut image = Box::new([0; BLOCK_SIZE]);
        loop {
            let generation = {
                let state = self.state.lock();
                if state.publishing {
                    return Err(Ext4Error::new(ErrCode::EAGAIN));
                }
                if let Some(block) = lookup(&state, home) {
                    image.copy_from_slice(block.bytes());
                    return Ok(Block::new(home, image));
                }
                state.generation
            };
            let disk = source.read_committed_metadata(home)?;
            let state = self.state.lock();
            if state.publishing {
                return Err(Ext4Error::new(ErrCode::EAGAIN));
            }
            if let Some(block) = lookup(&state, home) {
                image.copy_from_slice(block.bytes());
                return Ok(Block::new(home, image));
            }
            if state.generation == generation {
                return Ok(disk);
            }
        }
    }

    pub(super) fn resource_reusable(&self, range: RetiredRange) -> bool {
        let state = self.state.lock();
        reusable(&state.retired, range)
            && state
                .frozen
                .as_ref()
                .is_none_or(|batch| reusable(&batch.retired, range))
    }

    pub(super) fn resource_reuse_restart(&self, range: RetiredRange) -> Option<u64> {
        let state = self.state.lock();
        reuse_restart(&state.retired, range).max(
            state
                .frozen
                .as_ref()
                .and_then(|batch| reuse_restart(&batch.retired, range)),
        )
    }

    pub(super) fn request_retirement_progress(&self) -> bool {
        let mut state = self.state.lock();
        let pending = !state.retired.is_empty()
            || state
                .frozen
                .as_ref()
                .is_some_and(|batch| !batch.retired.is_empty());
        if pending && !state.running.is_empty() {
            state.seal_requested = true;
            state.generation = state.generation.wrapping_add(1);
        }
        drop(state);
        if pending {
            self.wake();
        }
        pending
    }

    pub fn block_range_reusable(&self, start: PBlockId, count: u64) -> Result<bool> {
        Ok(self.resource_reusable(RetiredRange::blocks(start, count)?))
    }

    pub fn inode_reusable(&self, id: InodeId) -> bool {
        self.resource_reusable(RetiredRange::inode(id))
    }

    /// Sealing is fair: no new operation starts until the current operation
    /// exits and the worker has frozen its final images.
    pub fn request_seal(&self) {
        let mut state = self.state.lock();
        if !state.running.is_empty() || state.active {
            state.seal_requested = true;
            state.generation = state.generation.wrapping_add(1);
        }
        drop(state);
        self.wake();
    }

    /// Stop admitting new operations; an already active operation may abort.
    /// Mount teardown must drain its upper-level work before closing admission.
    pub fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.seal_requested = !state.running.is_empty();
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.wake();
    }

    pub fn fail_stop(&self) {
        let mut state = self.state.lock();
        state.failed = true;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.wake();
    }

    /// Execute at most one requested batch. None means no work is ready; the
    /// mount waits for generation/deadline changes rather than busy polling.
    /// A returned disk error is always post-publication, regardless of its
    /// CommitFailure phase, so every such failure stops further publication.
    pub fn commit_pending(
        &self,
        device: &dyn BlockDevice,
    ) -> core::result::Result<Option<u64>, CommitError> {
        self.commit_pending_inner(device, None)
    }

    pub(super) fn commit_pending_with_publisher(
        &self,
        device: &dyn BlockDevice,
        publisher: &dyn CachePublisher,
    ) -> core::result::Result<Option<u64>, CommitError> {
        self.commit_pending_inner(device, Some(publisher))
    }

    fn commit_pending_inner(
        &self,
        device: &dyn BlockDevice,
        publisher: Option<&dyn CachePublisher>,
    ) -> core::result::Result<Option<u64>, CommitError> {
        let (mut frozen, sequence) = {
            let mut state = self.state.lock();
            if state.failed || state.committing || state.active || !state.seal_requested {
                return Ok(None);
            }
            if state.running.is_empty() {
                state.seal_requested = false;
                state.generation = state.generation.wrapping_add(1);
                drop(state);
                self.wake();
                return Ok(None);
            }
            debug_assert!(state.frozen.is_none());
            // Reuse the control block and both fixed-capacity owner vectors
            // reserved at mount; sealing itself performs no allocation.
            let mut owner = state.spare.take().expect("idle frozen owner");
            let frozen = Arc::get_mut(&mut owner).expect("unpublished frozen owner");
            core::mem::swap(&mut frozen.blocks, &mut state.running);
            core::mem::swap(&mut frozen.retired, &mut state.retired);
            frozen.operations = core::mem::take(&mut state.operations);
            state.frozen = Some(Arc::clone(&owner));
            let sequence = state.accepted;
            state.committing = true;
            state.seal_requested = false;
            state.generation = state.generation.wrapping_add(1);
            (owner, sequence)
        };
        self.wake();
        let mut images = Vec::new();
        if images.try_reserve_exact(frozen.blocks.len()).is_err() {
            self.fail_stop();
            return Err(CommitError {
                error: Ext4Error::new(ErrCode::ENOMEM),
                failure: super::journal_transaction::CommitFailure::BeforeCommit,
                poisoned: true,
            });
        }
        images.extend(frozen.blocks.iter());
        let result = self.journal.commit_batch_images(device, &images);
        drop(images);
        if let Err(mut error) = result {
            // Even a pre-I/O allocation/format failure happened after live
            // acceptance. Retain both overlays, wake everyone, never retry
            // leases or report the accepted sequence as durable.
            self.fail_stop();
            error.poisoned = true;
            return Err(error);
        }
        let old_owner = {
            let mut state = self.state.lock();
            state.durable = sequence;
            state.generation = state.generation.wrapping_add(1);
            state.frozen.take()
        };
        if let Some(publisher) = publisher {
            publisher.checkpoint(sequence, &frozen.blocks);
        }
        drop(old_owner);
        let recycled = Arc::get_mut(&mut frozen).expect("only worker owns retired frozen images");
        recycled.blocks.clear();
        recycled.retired.clear();
        recycled.operations = 0;
        let mut state = self.state.lock();
        state.spare = Some(frozen);
        state.committing = false;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.wake();
        Ok(Some(sequence))
    }
}

fn lookup(state: &State, home: PBlockId) -> Option<&StagedBlock> {
    if let Ok(index) = state.running.binary_search_by_key(&home, StagedBlock::home) {
        return Some(&state.running[index]);
    }
    let frozen = state.frozen.as_ref()?;
    frozen
        .blocks
        .binary_search_by_key(&home, StagedBlock::home)
        .ok()
        .map(|index| &frozen.blocks[index])
}
