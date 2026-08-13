//! Low-level operations of Ext4 filesystem.
//!
//! These interfaces are designed and arranged coresponding to FUSE low-level ops.
//! Ref: https://libfuse.github.io/doxygen/structfuse__lowlevel__ops.html

#[cfg(any(test, feature = "test-api"))]
use super::orphan::LegacyOrphanMembership;
use super::orphan::{final_unlink_orphan_action, FinalUnlinkOrphanAction};
use super::DelallocLease;
#[cfg(any(test, feature = "test-api"))]
use super::DelallocMappedWriteback;
use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use crate::return_error;
use core::cmp::min;

const DIRECT_RANGE_MIN_BLOCKS: usize = 4;
const DIRECT_RANGE_MAX_BLOCKS: usize = 256;
const DIRECT_RANGE_ZERO_CHUNK_BLOCKS: usize = 8;

enum TransactionalRangePrepare {
    Handled,
    Unsupported,
}

/// Outcome of the test-only mapped-tail data phase.
///
/// A block-device write can be retried with the same receipt: the extent and
/// linked orphan remain the authority for that receipt. Validation failures
/// instead prove that the raw primitive has lost its unique owner.
#[cfg(any(test, feature = "test-api"))]
enum DelallocMappedDataFailure {
    Retryable(Ext4Error),
    Fatal(Ext4Error),
}

struct TransactionalRangePlan {
    start_lblock: LBlockId,
    count: u32,
    preferred_first: Option<PBlockId>,
    requires_merge: bool,
    requires_root_split: bool,
    requires_leaf_split: bool,
    leaf_home: Option<PBlockId>,
}

/// One complete delayed-allocation writeback operation on a single ext4
/// block. Unlike the host-only mapped-tail receipt, this request has no
/// externally visible intermediate state: the mapper writes
/// initialized data before committing the allocation, extent and durable EOF
/// in one journal transaction.
///
/// `payload` is exactly one filesystem block beginning at `offset`.
/// `durable_eof` may stop inside that block when PageCache's stable snapshot
/// is partial, but it may never pass the mapped block.  The VFS computes it
/// from its committable-EOF ledger rather than from visible `i_size` alone.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
pub struct DelallocAppendBlockWriteback<'a> {
    pub offset: usize,
    pub payload: &'a [u8],
    pub durable_eof: u64,
    pub mtime: Option<u32>,
    pub ctime: Option<u32>,
}

/// Data and inode metadata which a production append capability publishes.
///
/// The inode and logical offset deliberately remain absent: they are fixed by
/// the opaque reservation's admission certificate and cannot be rebound by a
/// caller at submission time.
pub struct DelallocAppendBlockPublication<'a> {
    pub payload: &'a [u8],
    pub durable_eof: u64,
    pub mtime: Option<u32>,
    pub ctime: Option<u32>,
}

#[derive(Clone, Copy)]
struct DelallocAppendSubmitPolicy {
    strict_aligned: bool,
    journal_credits_bound: Option<usize>,
    terminal_on_contract_error: bool,
}

/// Opaque, single-use capability for the first production delayed-allocation
/// mapper shape.
///
/// This is deliberately not a generic [`DelallocLease`]: its private inner
/// lease was admitted under the target inode's direct-metadata exclusion and
/// is bound to one inode generation, one logical block and the on-disk EOF
/// observed at admission.  Host recovery callers can neither construct it
/// from an arbitrary lease nor choose a different inode/offset at submit
/// time.
///
/// The VFS still has to couple this capability to its lifecycle lease,
/// queue-head claim, PageCache dirty-generation ticket and truncate drain
/// before it makes a page dirty.  This type closes the lower-layer raw
/// `inode + request + lease` escape hatch; it does not pretend that the VFS
/// proof already exists.
#[must_use = "an append delayed-allocation reservation must be released or finalised"]
pub struct DelallocAppendBlockReservation {
    lease: DelallocLease,
    pool_checkpoint: Option<DelallocPoolCheckpoint>,
    journal_credits_bound: usize,
}

struct DelallocPoolCheckpoint {
    before: super::extent::ExtentRightSpineProjection,
    after: super::extent::ExtentRightSpineProjection,
    added_metadata_leases: usize,
}

impl core::fmt::Debug for DelallocAppendBlockReservation {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("DelallocAppendBlockReservation(..)")
    }
}

#[must_use = "an extent-node pool must be released or terminalised"]
pub struct DelallocExtentNodePool {
    inode_id: InodeId,
    inode_generation: u32,
    projection: super::extent::ExtentRightSpineProjection,
    leases: Vec<DelallocLease>,
}

impl core::fmt::Debug for DelallocExtentNodePool {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("DelallocExtentNodePool(..)")
    }
}

impl DelallocAppendBlockReservation {
    fn new(lease: DelallocLease) -> Self {
        Self {
            lease,
            pool_checkpoint: None,
            journal_credits_bound: DELALLOC_APPEND_MAX_JOURNAL_CREDITS,
        }
    }

    /// This stays private to the ext4 crate.  The host recovery API below is
    /// the only compatibility caller that may unwrap the old raw test
    /// carrier.  The future VFS token will adopt this shape only together
    /// with its lifecycle, queue, EOF and drain proof.
    #[cfg(any(test, feature = "test-api"))]
    fn into_host_test_lease(self) -> DelallocLease {
        self.lease
    }
}

// Extent validation accepts depths 0..=5. Appending through a completely full
// right spine can allocate one node at every existing level plus a promoted
// root, hence at most six new extent-tree blocks in one submission.
const DELALLOC_APPEND_MAX_NEW_EXTENT_NODES: usize = 6;
// Home-image bound for one append:
//   10 = inode/path plus the data allocation bitmap/GDT/superblock envelope;
//   4 per new node = node image plus its allocation bitmap/GDT/superblock.
// Allocation groups are chosen only after the transaction starts, so admission
// cannot know an exact home set. This checked single-submit upper bound is the
// proof carried by the private reservation.
const DELALLOC_APPEND_MAX_JOURNAL_CREDITS: usize = 10 + 4 * DELALLOC_APPEND_MAX_NEW_EXTENT_NODES;

fn delalloc_append_journal_credits(metadata_blocks: usize) -> Result<usize> {
    10usize
        .checked_add(
            metadata_blocks
                .checked_mul(4)
                .ok_or_else(|| Ext4Error::new(ErrCode::E2BIG))?,
        )
        .ok_or_else(|| Ext4Error::new(ErrCode::E2BIG))
}

fn delalloc_append_batch_journal_credits(blocks: usize) -> Result<usize> {
    DELALLOC_APPEND_MAX_JOURNAL_CREDITS
        .checked_mul(blocks)
        .ok_or_else(|| Ext4Error::new(ErrCode::E2BIG))
}

fn validate_delalloc_journal_credit_bound(actual: usize, bound: Option<usize>) -> Result<usize> {
    match bound {
        Some(bound) if actual <= bound => Ok(bound),
        // A private production certificate is admitted only after proving
        // this bound.  Exceeding it is a deterministic capability-contract
        // violation, not an I/O EIO that may be retried.
        Some(_) => Err(Ext4Error::new(ErrCode::E2BIG)),
        None => Ok(actual),
    }
}

fn is_delalloc_contract_error(code: ErrCode) -> bool {
    matches!(
        code,
        ErrCode::EINVAL
            | ErrCode::ENOTSUP
            | ErrCode::EFBIG
            | ErrCode::ERANGE
            | ErrCode::E2BIG
            | ErrCode::ENOSPC
            | ErrCode::EROFS
    )
}

/// Mount-scoped authority for the bounded production append mapper.
///
/// Callers cannot construct, clone, or inspect this value.  Every production
/// reserve/submit/release terminal validates it against the issuing `Ext4`
/// instance, so a typed reservation cannot be rebound through a foreign
/// superblock.  The DragonOS VFS keeps one authority in the canonical
/// filesystem object and combines it with its inode/PageCache lifecycle proof.
#[must_use = "the mapper authority must remain owned by its filesystem"]
pub struct DelallocAppendMapperAuthority {
    mount_generation: u64,
}

impl core::fmt::Debug for DelallocAppendMapperAuthority {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("DelallocAppendMapperAuthority(..)")
    }
}

/// Publication-aware result of the bounded append mapper.
///
/// `RetryableNotPublished` is deliberately not a user-visible retry hint: it
/// proves that the exact lease remains active and the caller must restore its
/// queue claim before PageCache redirties the batch. `Terminal` means the
/// mapper either crossed a commit attempt or observed an already fail-stopped
/// mount and terminalised the lease. Recovery, rather than the caller, owns
/// any tail in both cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelallocAppendBlockSubmitOutcome {
    Completed,
    RetryableNotPublished(ErrCode),
    Terminal(ErrCode),
}

/// Attributes that can be set on an inode via `setattr`.
#[derive(Default)]
pub struct SetAttr {
    /// File mode and permissions
    pub mode: Option<InodeMode>,
    /// 32-bit user id
    pub uid: Option<u32>,
    /// 32-bit group id
    pub gid: Option<u32>,
    /// 64-bit file size
    pub size: Option<u64>,
    /// 32-bit access time in seconds
    pub atime: Option<u32>,
    /// 32-bit modify time in seconds
    pub mtime: Option<u32>,
    /// 32-bit change time in seconds
    pub ctime: Option<u32>,
    /// 32-bit create time in seconds
    pub crtime: Option<u32>,
}

#[derive(Clone, Copy)]
pub struct InodeOwner {
    pub uid: u32,
    pub gid: u32,
}

impl SetAttr {
    /// Create a new SetAttr struct with all fields set to None.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Ext4 {
    /// Build the bounded one-block append plan. This deliberately keeps
    /// the first mapper small enough that a successful reservation cannot
    /// turn into a writeback-time metadata `ENOSPC`:
    /// root/leaf growth receives one explicit metadata credit, while shapes
    /// that require a physically adjacent run are deferred to the later range
    /// mapper instead of being guessed from aggregate free space.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    fn delalloc_append_block_plan(
        &self,
        inode: &InodeRef,
        offset: usize,
    ) -> Result<(TransactionalRangePlan, u64)> {
        if !offset.is_multiple_of(BLOCK_SIZE) {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        // This bounded primitive can bridge exactly the one partial block at
        // the current EOF.  Supporting a larger preallocated/sparse gap would
        // require one durable zero proof per intervening block, which belongs
        // to the later range token rather than an aggregate single-block
        // reservation.
        let next_lblock = inode
            .inode
            .size()
            .checked_add(BLOCK_SIZE as u64 - 1)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?
            / BLOCK_SIZE as u64;
        let append_offset = next_lblock
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        if u64::try_from(offset).map_err(|_| Ext4Error::new(ErrCode::EFBIG))? != append_offset {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let start_lblock =
            u32::try_from(offset / BLOCK_SIZE).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        let shape = self
            .direct_append_shape(inode, start_lblock, 1)?
            .ok_or_else(|| Ext4Error::new(ErrCode::ENOTSUP))?;
        if shape.requires_merge {
            // A full unsupported leaf can only accept an append when the
            // physical block directly follows its tail. A logical reservation
            // does not pin that run, so accepting it would manufacture a late
            // writeback ENOSPC after userspace observed a successful write.
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let metadata_blocks = u64::from(shape.requires_root_split)
            .checked_add(u64::from(shape.requires_leaf_split))
            .ok_or_else(|| Ext4Error::new(ErrCode::ERANGE))?;
        Ok((
            TransactionalRangePlan {
                start_lblock,
                count: 1,
                preferred_first: shape.preferred_first,
                requires_merge: false,
                requires_root_split: shape.requires_root_split,
                requires_leaf_split: shape.requires_leaf_split,
                leaf_home: shape.leaf_home,
            },
            metadata_blocks,
        ))
    }

    /// Admission projection for a follower whose predecessors are still only
    /// represented by the inode queue. Stage 3c-1 admits followers only while
    /// every projected non-merging extent fits in the current right-most
    /// leaf; right-spine growth is added by the next implementation phase.
    fn delalloc_projected_append_block_plan(
        &self,
        inode: &InodeRef,
        offset: usize,
        expected_durable_eof_before: u64,
    ) -> Result<(TransactionalRangePlan, u64)> {
        if expected_durable_eof_before == inode.inode.size() {
            return self.delalloc_append_block_plan(inode, offset);
        }
        let append_offset = expected_durable_eof_before
            .checked_add(BLOCK_SIZE as u64 - 1)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?
            / BLOCK_SIZE as u64
            * BLOCK_SIZE as u64;
        if expected_durable_eof_before < inode.inode.size()
            || u64::try_from(offset).map_err(|_| Ext4Error::new(ErrCode::EFBIG))? != append_offset
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let start_lblock =
            u32::try_from(offset / BLOCK_SIZE).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        let (on_disk_next_lblock, free_entries) = self.extent_rightmost_append_capacity(inode)?;
        let projected_predecessors = start_lblock
            .checked_sub(on_disk_next_lblock)
            .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?
            as usize;
        if projected_predecessors >= free_entries {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        Ok((
            TransactionalRangePlan {
                start_lblock,
                count: 1,
                preferred_first: None,
                requires_merge: false,
                requires_root_split: false,
                requires_leaf_split: false,
                leaf_home: None,
            },
            0,
        ))
    }

    /// Make the old partial EOF block safe to expose before an append grows
    /// the file into the next filesystem block.  A truncate may leave the
    /// physical block allocated with bytes beyond `i_size`; Linux similarly
    /// zeroes that partial block before extending the file.  The bounded
    /// A sparse write may either allocate the currently-unmapped block which
    /// contains EOF or a later right-edge block.  If the EOF block is already
    /// mapped, only a later block is eligible and its old tail must be zeroed.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    fn zero_delalloc_append_eof_tail(
        &self,
        inode: &InodeRef,
        old_eof: u64,
        offset: usize,
    ) -> Result<()> {
        if self.write_delalloc_append_eof_tail(inode, old_eof, offset)? {
            self.block_device.flush()?;
        }
        Ok(())
    }

    /// Write the old partial-EOF tail without creating its own durability
    /// boundary. Batched delayed writeback includes this data write in the
    /// single flush which precedes mapping publication.
    fn write_delalloc_append_eof_tail(
        &self,
        inode: &InodeRef,
        old_eof: u64,
        offset: usize,
    ) -> Result<bool> {
        let tail_offset = usize::try_from(old_eof % BLOCK_SIZE as u64)
            .map_err(|_| Ext4Error::new(ErrCode::ERANGE))?;
        if old_eof == 0 || tail_offset == 0 {
            return Ok(false);
        }
        let target_lblock =
            u32::try_from(offset / BLOCK_SIZE).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        let eof_lblock = u32::try_from(old_eof / BLOCK_SIZE as u64)
            .map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        if target_lblock < eof_lblock {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let pblock = match self.extent_query(inode, eof_lblock) {
            Ok(pblock) => pblock,
            Err(error) if error.code() == ErrCode::ENOENT => return Ok(false),
            Err(error) => return Err(error),
        };
        if target_lblock == eof_lblock {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let mut block = self.read_block(pblock)?;
        block.data[tail_offset..].fill(0);
        self.write_block(&block)?;
        Ok(true)
    }

    /// Reserve the first production mapper shape as an opaque capability.
    ///
    /// The returned capability captures the canonical inode/range provenance
    /// while the direct-metadata domain and this inode's mutation shard are
    /// both held.  Its private lease cannot subsequently be rebound to a
    /// different append request.  A future VFS token must still add its
    /// lifecycle, queue-head, EOF-ticket and truncate-drain proof before it
    /// makes a dirty page visible.
    fn reserve_delalloc_append_block_capability_inner(
        &self,
        id: InodeId,
        offset: usize,
        expected_durable_eof_before: Option<u64>,
    ) -> Result<DelallocAppendBlockReservation> {
        self.ensure_mutable()?;
        if !self.uses_journal() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let inode = self.read_inode(id)?;
        if !inode.inode.is_file() || !inode.inode.uses_extents() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let expected_durable_eof_before =
            expected_durable_eof_before.unwrap_or_else(|| inode.inode.size());
        let (_plan, metadata_blocks) =
            self.delalloc_projected_append_block_plan(&inode, offset, expected_durable_eof_before)?;
        let mut lease =
            self.reserve_delalloc_lease_in_direct_mutation_domain(1, metadata_blocks)?;
        lease
            .bind_append_block_certificate(
                id,
                inode.inode.generation(),
                offset,
                expected_durable_eof_before,
            )
            .expect("fresh delayed-allocation append lease must accept one certificate");
        Ok(DelallocAppendBlockReservation::new(lease))
    }

    /// Issue the normal-build mount authority for the bounded append mapper.
    pub fn delalloc_append_mapper_authority(&self) -> Result<DelallocAppendMapperAuthority> {
        self.ensure_mutable()?;
        if !self.uses_journal() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        self.delalloc_mapper_authority_issued
            .compare_exchange(
                false,
                true,
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| Ext4Error::new(ErrCode::EEXIST))?;
        Ok(DelallocAppendMapperAuthority {
            mount_generation: self.delalloc_mount_generation(),
        })
    }

    fn validate_delalloc_append_mapper_authority(
        &self,
        authority: &DelallocAppendMapperAuthority,
    ) -> Result<()> {
        self.ensure_mutable()?;
        if !self.uses_journal() || authority.mount_generation != self.delalloc_mount_generation() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        Ok(())
    }

    pub fn create_delalloc_extent_node_pool_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
        id: InodeId,
    ) -> Result<DelallocExtentNodePool> {
        self.validate_delalloc_append_mapper_authority(authority)?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let inode = self.read_inode(id)?;
        if !inode.inode.is_file() || !inode.inode.uses_extents() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        Ok(DelallocExtentNodePool {
            inode_id: id,
            inode_generation: inode.inode.generation(),
            projection: self.extent_right_spine_projection(&inode)?,
            leases: Vec::new(),
        })
    }

    pub fn release_delalloc_extent_node_pool_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
        pool: &mut DelallocExtentNodePool,
    ) -> Result<()> {
        self.validate_delalloc_append_mapper_authority(authority)?;
        let mut leases: Vec<&mut DelallocLease> = pool.leases.iter_mut().collect();
        self.release_delalloc_lease_batch(&mut leases)?;
        pool.leases.clear();
        Ok(())
    }

    pub fn terminalize_delalloc_extent_node_pool_authorized_after_fail_stop(
        &self,
        authority: &DelallocAppendMapperAuthority,
        pool: &mut DelallocExtentNodePool,
    ) -> Result<()> {
        if authority.mount_generation != self.delalloc_mount_generation()
            || pool
                .leases
                .iter()
                .any(|lease| !lease.belongs_to_mount(self.delalloc_mount_generation()))
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.poison(ErrCode::EIO);
        for lease in &mut pool.leases {
            if lease.active {
                lease.deactivate();
            }
        }
        pool.leases.clear();
        Ok(())
    }

    /// Reserve one aligned append block for the production VFS bridge.
    pub fn reserve_delalloc_append_block_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
        id: InodeId,
        offset: usize,
    ) -> Result<DelallocAppendBlockReservation> {
        self.validate_delalloc_append_mapper_authority(authority)?;
        self.reserve_delalloc_append_block_capability_inner(id, offset, None)
    }

    /// Reserve a follower against the inode queue's exact durable-EOF
    /// predecessor. The certificate remains opaque and submit validates the
    /// actual on-disk EOF after all older entries complete.
    pub fn reserve_delalloc_append_block_projected_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
        id: InodeId,
        offset: usize,
        expected_durable_eof_before: u64,
        pool: &mut DelallocExtentNodePool,
    ) -> Result<DelallocAppendBlockReservation> {
        self.validate_delalloc_append_mapper_authority(authority)?;
        if pool.inode_id != id {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if !self.transaction_credits_fit(DELALLOC_APPEND_MAX_JOURNAL_CREDITS)? {
            // This journal geometry cannot ever materialize the admitted
            // right-spine shape. Reject before reserving blocks or publishing
            // a Dirty page; writeback must never discover deterministic
            // E2BIG and spin it as EAGAIN.
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }

        let (inode_generation, expected_lblock, mut projection) = {
            let _metadata_guard = self.lock_direct_metadata_mutation()?;
            let _mutation_guard =
                self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
            let inode = self.read_inode(id)?;
            if inode.inode.generation() != pool.inode_generation {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            let expected_lblock =
                u32::try_from(offset / BLOCK_SIZE).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
            let minimum_lblock = u32::try_from(expected_durable_eof_before / BLOCK_SIZE as u64)
                .map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
            if !offset.is_multiple_of(BLOCK_SIZE)
                || expected_lblock < minimum_lblock
                || pool.projection.next_lblock > expected_lblock
            {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            (
                inode.inode.generation(),
                expected_lblock,
                pool.projection.clone(),
            )
        };
        let new_nodes = projection.append_nonmerge_at(expected_lblock, 1)?;

        let mut data_lease = self.reserve_delalloc_lease(1, 0)?;
        if let Err(error) = data_lease.bind_append_block_certificate(
            id,
            inode_generation,
            offset,
            expected_durable_eof_before,
        ) {
            let _ = self.release_delalloc_lease_batch(&mut [&mut data_lease]);
            return Err(error);
        }
        let mut new_metadata = Vec::new();
        for _ in 0..new_nodes {
            match self.reserve_delalloc_lease(0, 1) {
                Ok(lease) => new_metadata.push(lease),
                Err(error) => {
                    let mut leases: Vec<&mut DelallocLease> = new_metadata.iter_mut().collect();
                    leases.push(&mut data_lease);
                    if self.release_delalloc_lease_batch(&mut leases).is_err() {
                        self.poison(ErrCode::EIO);
                        for lease in &mut new_metadata {
                            lease.deactivate();
                        }
                        data_lease.deactivate();
                        return Err(Ext4Error::new(ErrCode::EIO));
                    }
                    return Err(error);
                }
            }
        }
        let checkpoint = DelallocPoolCheckpoint {
            before: core::mem::replace(&mut pool.projection, projection.clone()),
            after: projection,
            added_metadata_leases: new_metadata.len(),
        };
        pool.leases.extend(new_metadata);
        Ok(DelallocAppendBlockReservation {
            lease: data_lease,
            pool_checkpoint: Some(checkpoint),
            journal_credits_bound: DELALLOC_APPEND_MAX_JOURNAL_CREDITS,
        })
    }

    #[cfg(any(test, feature = "test-api"))]
    pub fn reserve_delalloc_append_block_capability(
        &self,
        id: InodeId,
        offset: usize,
    ) -> Result<DelallocAppendBlockReservation> {
        self.reserve_delalloc_append_block_capability_inner(id, offset, None)
    }

    /// Host-recovery compatibility wrapper for the pre-capability reserve
    /// API.  It intentionally remains unavailable to normal DragonOS builds:
    /// the raw lease does not carry the VFS proof required for production
    /// PageCache writeback.
    #[cfg(any(test, feature = "test-api"))]
    pub fn reserve_delalloc_append_block(
        &self,
        id: InodeId,
        offset: usize,
    ) -> Result<DelallocLease> {
        self.reserve_delalloc_append_block_capability_inner(id, offset, None)
            .map(DelallocAppendBlockReservation::into_host_test_lease)
    }

    fn abort_delalloc_append_block_transaction_many(
        &self,
        transaction: super::journal_transaction::Transaction<'_>,
        data: super::alloc::DelallocConsumption,
        metadata: Vec<super::alloc::DelallocConsumption>,
    ) -> Result<()> {
        transaction.abort();
        let mut failure = None;
        for consumption in metadata.into_iter().rev() {
            if let Err(error) = self.rollback_delalloc_allocation(consumption) {
                failure.get_or_insert(error);
            }
        }
        if let Err(error) = self.rollback_delalloc_allocation(data) {
            failure.get_or_insert(error);
        }
        failure.map_or(Ok(()), Err)
    }

    fn finish_unpublished_delalloc_error(
        &self,
        cleanup: Result<()>,
        lease: &mut DelallocLease,
        original: Ext4Error,
    ) -> Result<()> {
        if cleanup.is_ok() {
            return Err(original);
        }
        self.poison(ErrCode::EIO);
        lease.deactivate();
        Err(Ext4Error::new(ErrCode::EIO))
    }

    fn rollback_delalloc_consumptions_many(
        &self,
        data: super::alloc::DelallocConsumption,
        metadata: Vec<super::alloc::DelallocConsumption>,
    ) -> Result<()> {
        let mut failure = None;
        for consumption in metadata.into_iter().rev() {
            if let Err(error) = self.rollback_delalloc_allocation(consumption) {
                failure.get_or_insert(error);
            }
        }
        if let Err(error) = self.rollback_delalloc_allocation(data) {
            failure.get_or_insert(error);
        }
        failure.map_or(Ok(()), Err)
    }

    fn rollback_delalloc_consumption_vectors(
        &self,
        data: Vec<super::alloc::DelallocConsumption>,
        metadata: Vec<super::alloc::DelallocConsumption>,
    ) -> Result<()> {
        let mut failure = None;
        for consumption in metadata.into_iter().rev().chain(data.into_iter().rev()) {
            if let Err(error) = self.rollback_delalloc_allocation(consumption) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn finalize_delalloc_append_consumptions(
        &self,
        lease: &mut DelallocLease,
        data: &mut super::alloc::DelallocConsumption,
        metadata: &mut Vec<super::alloc::DelallocConsumption>,
        pool: Option<&mut DelallocExtentNodePool>,
        pool_indices: Vec<usize>,
    ) -> Result<()> {
        let Some(pool) = pool else {
            if metadata.len() > 1 || !pool_indices.is_empty() {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            return self.finalize_delalloc_append_block(lease, data, metadata.first_mut());
        };

        if pool_indices.len() != metadata.len() {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        self.finalize_delalloc_append_block_with_pool(lease, data, None)?;
        for (index, consumption) in pool_indices.iter().copied().zip(metadata.drain(..)) {
            self.commit_delalloc_allocation(consumption, &mut pool.leases[index])?;
        }
        for index in pool_indices.into_iter().rev() {
            drop(pool.leases.swap_remove(index));
        }
        Ok(())
    }

    /// Materialise, initialize and submit one previously reserved append
    /// block, then publish the extent and the caller-proven durable EOF in one
    /// journal transaction.
    ///
    /// Data I/O is flushed before the journal commit.  Therefore a crash
    /// before a commit record leaves an unreachable (and reusable) block,
    /// while a replayed commit can only reveal initialized payload.  A commit
    /// outcome after the publication point fail-stops the mount and finalises
    /// the lease; an ordinary data I/O failure aborts the transaction and
    /// leaves the lease live so PageCache may re-dirty and retry it.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    fn submit_delalloc_append_block_inner(
        &self,
        id: InodeId,
        request: DelallocAppendBlockWriteback<'_>,
        lease: &mut DelallocLease,
        pool: Option<&mut DelallocExtentNodePool>,
        strict_aligned: bool,
        journal_credits_bound: Option<usize>,
    ) -> Result<()> {
        let mut pool = pool;
        self.ensure_mutable()?;
        if !self.uses_journal()
            || !request.offset.is_multiple_of(BLOCK_SIZE)
            || request.payload.is_empty()
            || request.payload.len() > BLOCK_SIZE
            || !lease.active
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let block_end = request
            .offset
            .checked_add(BLOCK_SIZE)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        let block_end_u64 = u64::try_from(block_end).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        if request.durable_eof <= request.offset as u64
            || request.durable_eof > block_end_u64
            || (strict_aligned && request.durable_eof != block_end_u64)
        {
            return Err(Ext4Error::new(if strict_aligned {
                ErrCode::EINVAL
            } else {
                ErrCode::EAGAIN
            }));
        }
        let visible_len = usize::try_from(request.durable_eof - request.offset as u64)
            .map_err(|_| Ext4Error::new(ErrCode::ERANGE))?;
        if request.payload.len() != visible_len {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }

        let certificate = lease
            .append_block_certificate()
            .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
        if certificate.inode_id != id
            || certificate.offset != request.offset
            || lease.data_blocks != 1
        {
            return Err(Ext4Error::new(if strict_aligned {
                ErrCode::EINVAL
            } else {
                ErrCode::EAGAIN
            }));
        }

        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        let on_disk_eof = inode.inode.size();
        if !inode.inode.is_file()
            || !inode.inode.uses_extents()
            || inode.inode.generation() != certificate.inode_generation
            || on_disk_eof != certificate.expected_durable_eof_before
            || (strict_aligned
                && (on_disk_eof != request.offset as u64 || on_disk_eof % BLOCK_SIZE as u64 != 0))
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let start_lblock = u32::try_from(request.offset / BLOCK_SIZE)
            .map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        let extent_plan = self.right_spine_append_plan(&inode, start_lblock)?;
        let projected_metadata_blocks = extent_plan.new_nodes();
        if pool.as_ref().is_some_and(|pool| {
            pool.inode_id != id || pool.inode_generation != inode.inode.generation()
        }) || (projected_metadata_blocks != 0
            && match pool.as_ref() {
                Some(pool) => {
                    pool.leases.iter().filter(|lease| lease.active).count()
                        < projected_metadata_blocks
                }
                None => lease.metadata_blocks < projected_metadata_blocks as u64,
            })
        {
            // The VFS must drain conflicting eager/mapped work before it
            // reuses a certificate.  Do not reinterpret a reservation with a
            // larger metadata need as a retryable ENOSPC.
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }

        let actual_credits = delalloc_append_journal_credits(projected_metadata_blocks)?;
        let credits =
            validate_delalloc_journal_credit_bound(actual_credits, journal_credits_bound)?;
        let mut transaction = self.transaction_start(credits)?;
        let (allocation, data_consumption) = match self.transaction_alloc_delalloc_range(
            &mut transaction,
            id,
            extent_plan.preferred_first,
            false,
            1,
            lease,
        ) {
            Ok(allocation) => allocation,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        // Admission reserves the non-merge right-spine upper bound. The data
        // allocator can now prove whether the actual physical block extends
        // the current tail; a merge consumes no metadata-node lease even when
        // the leaf is full, leaving that conservative pool capacity for a
        // later non-contiguous follower.
        let metadata_blocks = if extent_plan.can_merge(allocation.first, 1) {
            0
        } else {
            projected_metadata_blocks
        };
        let mut metadata_consumptions = Vec::new();
        let mut metadata_pool_indices = Vec::new();
        let mut metadata_homes = Vec::new();
        if metadata_blocks != 0 {
            let selected: Vec<Option<usize>> = if let Some(pool) = pool.as_deref() {
                pool.leases
                    .iter()
                    .enumerate()
                    .filter_map(|(index, lease)| lease.active.then_some(Some(index)))
                    .take(metadata_blocks)
                    .collect()
            } else {
                if metadata_blocks != 1 || lease.metadata_blocks < 1 {
                    return Err(Ext4Error::new(ErrCode::EINVAL));
                }
                vec![None]
            };
            for pool_index in selected {
                let metadata_lease = match pool_index {
                    Some(index) => {
                        &mut pool
                            .as_deref_mut()
                            .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?
                            .leases[index]
                    }
                    None => &mut *lease,
                };
                let (metadata_allocation, consumption) = match self
                    .transaction_alloc_delalloc_metadata_block(
                        &mut transaction,
                        id,
                        Some(allocation.first),
                        metadata_lease,
                    ) {
                    Ok(allocation) => allocation,
                    Err(error) => {
                        let cleanup = self.abort_delalloc_append_block_transaction_many(
                            transaction,
                            data_consumption,
                            metadata_consumptions,
                        );
                        return self.finish_unpublished_delalloc_error(cleanup, lease, error);
                    }
                };
                if let Some(index) = pool_index {
                    metadata_pool_indices.push(index);
                }
                metadata_homes.push(metadata_allocation.first);
                metadata_consumptions.push(consumption);
            }
            if metadata_homes.len() != metadata_blocks {
                let cleanup = self.abort_delalloc_append_block_transaction_many(
                    transaction,
                    data_consumption,
                    metadata_consumptions,
                );
                return self.finish_unpublished_delalloc_error(
                    cleanup,
                    lease,
                    Ext4Error::new(ErrCode::EIO),
                );
            }
        }

        let mut initialized = [0u8; BLOCK_SIZE];
        initialized[..visible_len].copy_from_slice(request.payload);
        if !strict_aligned {
            if let Err(error) =
                self.zero_delalloc_append_eof_tail(&inode, on_disk_eof, request.offset)
            {
                let cleanup = self.abort_delalloc_append_block_transaction_many(
                    transaction,
                    data_consumption,
                    metadata_consumptions,
                );
                return self.finish_unpublished_delalloc_error(cleanup, lease, error);
            }
        }
        // `initialized` is already a complete zero-padded block. Persist it
        // exactly once; a separate pre-zero write doubles I/O without adding
        // a recovery guarantee because the extent is still unpublished.
        if let Err(error) = self
            .block_device
            .write_blocks(allocation.first, &initialized)
        {
            let cleanup = self.abort_delalloc_append_block_transaction_many(
                transaction,
                data_consumption,
                metadata_consumptions,
            );
            return self.finish_unpublished_delalloc_error(cleanup, lease, error);
        }
        if let Err(error) = self.block_device.flush() {
            let cleanup = self.abort_delalloc_append_block_transaction_many(
                transaction,
                data_consumption,
                metadata_consumptions,
            );
            return self.finish_unpublished_delalloc_error(cleanup, lease, error);
        }

        if let Err(error) = self.stage_journaled_right_spine_append(
            &mut transaction,
            &mut inode,
            &extent_plan,
            &metadata_homes,
            allocation.first,
            1,
        ) {
            let cleanup = self.abort_delalloc_append_block_transaction_many(
                transaction,
                data_consumption,
                metadata_consumptions,
            );
            return self.finish_unpublished_delalloc_error(cleanup, lease, error);
        }
        inode.inode.set_size(request.durable_eof);
        if let Some(mtime) = request.mtime {
            inode.inode.set_mtime(mtime);
        }
        if let Some(ctime) = request.ctime {
            inode.inode.set_ctime(ctime);
        }
        if let Err(error) = self.transaction_stage_inode_with_csum(&mut transaction, &mut inode) {
            let cleanup = self.abort_delalloc_append_block_transaction_many(
                transaction,
                data_consumption,
                metadata_consumptions,
            );
            return self.finish_unpublished_delalloc_error(cleanup, lease, error);
        }

        match transaction.commit(self.block_device.as_ref(), self) {
            Ok(()) => {
                let mut data_consumption = data_consumption;
                if self
                    .finalize_delalloc_append_consumptions(
                        lease,
                        &mut data_consumption,
                        &mut metadata_consumptions,
                        pool.as_deref_mut(),
                        metadata_pool_indices,
                    )
                    .is_err()
                {
                    self.poison(ErrCode::EIO);
                    return Err(Ext4Error::new(ErrCode::EIO));
                }
                Ok(())
            }
            Err(error) => {
                if error.failure == super::journal_transaction::CommitFailure::BeforeCommit {
                    if self
                        .rollback_delalloc_consumptions_many(
                            data_consumption,
                            metadata_consumptions,
                        )
                        .is_err()
                    {
                        self.poison(ErrCode::EIO);
                        lease.deactivate();
                        return Err(Ext4Error::new(ErrCode::EIO));
                    }
                    if error.poisoned {
                        // No commit record exists, but journal I/O has made
                        // the core unusable. The consumption is back in this
                        // exact lease; terminalise it without restoring
                        // capacity to a mount which can no longer allocate.
                        self.poison(ErrCode::EIO);
                        lease.deactivate();
                        return Err(Ext4Error::new(ErrCode::EIO));
                    }
                    // Pre-commit validation/capacity failure left the journal
                    // core usable. Preserve the same active lease serial so
                    // PageCache can redirty and retry this exact queue entry.
                    return Err(error.error);
                } else {
                    let mut data_consumption = data_consumption;
                    if self
                        .finalize_delalloc_append_consumptions(
                            lease,
                            &mut data_consumption,
                            &mut metadata_consumptions,
                            pool,
                            metadata_pool_indices,
                        )
                        .is_err()
                    {
                        self.poison(ErrCode::EIO);
                        return Err(Ext4Error::new(ErrCode::EIO));
                    }
                }
                self.poison(ErrCode::EIO);
                Err(error.error)
            }
        }
    }

    /// Submit an opaque append capability and report whether its ownership
    /// remains retryable or has crossed the terminal journal boundary.
    ///
    /// Neither the caller nor this API can substitute a different inode or
    /// logical offset: both are read from the private admission certificate.
    /// `durable_eof` remains a VFS-proven input, because only the VFS owns the
    /// committable-EOF ledger.  Callers must advance that ledger only for
    /// [`DelallocAppendBlockSubmitOutcome::Completed`].
    fn submit_delalloc_append_block_capability_inner(
        &self,
        reservation: &mut DelallocAppendBlockReservation,
        publication: DelallocAppendBlockPublication<'_>,
        pool: Option<&mut DelallocExtentNodePool>,
        strict_aligned: bool,
        terminal_on_contract_error: bool,
    ) -> DelallocAppendBlockSubmitOutcome {
        let Some(certificate) = reservation.lease.append_block_certificate() else {
            // A typed reservation without its immutable certificate proves
            // internal capability corruption.  Do not panic in writeback: if
            // this mount owns the token, fail-stop it and consume the only
            // remaining owner.  A foreign token must remain untouched.
            if terminal_on_contract_error
                && reservation
                    .lease
                    .belongs_to_mount(self.delalloc_mount_generation())
            {
                self.poison(ErrCode::EIO);
                if reservation.lease.active {
                    reservation.lease.deactivate();
                }
                return DelallocAppendBlockSubmitOutcome::Terminal(ErrCode::EIO);
            }
            return DelallocAppendBlockSubmitOutcome::RetryableNotPublished(ErrCode::EINVAL);
        };
        let request = DelallocAppendBlockWriteback {
            offset: certificate.offset,
            payload: publication.payload,
            durable_eof: publication.durable_eof,
            mtime: publication.mtime,
            ctime: publication.ctime,
        };
        let journal_credits_bound = reservation.journal_credits_bound;
        let outcome = self.submit_delalloc_append_block_with_lease(
            certificate.inode_id,
            request,
            &mut reservation.lease,
            pool,
            DelallocAppendSubmitPolicy {
                strict_aligned,
                journal_credits_bound: Some(journal_credits_bound),
                terminal_on_contract_error,
            },
        );
        if !matches!(
            outcome,
            DelallocAppendBlockSubmitOutcome::RetryableNotPublished(_)
        ) {
            reservation.pool_checkpoint = None;
        }
        outcome
    }

    /// Return the largest batch whose worst-case fragmented metadata footprint
    /// fits the mounted journal. The bound is established before PageCache
    /// freezes a descriptor; lower submission never shortens a claimed batch.
    pub fn max_delalloc_append_batch_blocks_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
    ) -> Result<usize> {
        self.validate_delalloc_append_mapper_authority(authority)?;
        for blocks in (1..=64).rev() {
            let credits = delalloc_append_batch_journal_credits(blocks)?;
            if self.transaction_credits_fit(credits)? {
                return Ok(blocks);
            }
        }
        Err(Ext4Error::new(ErrCode::E2BIG))
    }

    /// Materialise a FIFO append batch from independent per-block
    /// capabilities. Physical continuity is only an optimisation: every
    /// block is allocated from its own admitted lease and consecutive
    /// allocations are appended through the transaction-private extent view.
    fn submit_delalloc_append_batch_inner(
        &self,
        reservations: &mut [&mut DelallocAppendBlockReservation],
        publications: &[DelallocAppendBlockPublication<'_>],
        pool: &mut DelallocExtentNodePool,
    ) -> Result<()> {
        self.ensure_mutable()?;
        if !self.uses_journal()
            || reservations.is_empty()
            || reservations.len() != publications.len()
            || reservations.len() > 64
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }

        let mut certificates = Vec::new();
        let mut initialized_blocks = Vec::new();
        certificates
            .try_reserve_exact(reservations.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        initialized_blocks
            .try_reserve_exact(reservations.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        for (index, (reservation, publication)) in
            reservations.iter().zip(publications.iter()).enumerate()
        {
            let certificate = reservation
                .lease
                .append_block_certificate()
                .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
            if !reservation.lease.active
                || reservation.lease.data_blocks != 1
                || publication.payload.is_empty()
                || publication.payload.len() > BLOCK_SIZE
                || publication.durable_eof <= certificate.offset as u64
                || publication.durable_eof
                    > certificate
                        .offset
                        .checked_add(BLOCK_SIZE)
                        .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))? as u64
                || publication.payload.len() as u64
                    != publication.durable_eof - certificate.offset as u64
            {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            if let Some(previous) = certificates.last() {
                let previous: &super::DelallocAppendBlockCertificate = previous;
                if certificate.inode_id != previous.inode_id
                    || certificate.inode_generation != previous.inode_generation
                    || certificate.offset
                        != previous
                            .offset
                            .checked_add(BLOCK_SIZE)
                            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?
                    || certificate.expected_durable_eof_before
                        != publications[index - 1].durable_eof
                {
                    return Err(Ext4Error::new(ErrCode::EINVAL));
                }
            }
            let mut initialized = [0u8; BLOCK_SIZE];
            initialized[..publication.payload.len()].copy_from_slice(publication.payload);
            initialized_blocks.push(initialized);
            certificates.push(certificate);
        }
        let first_certificate = certificates[0];
        if pool.inode_id != first_certificate.inode_id
            || pool.inode_generation != first_certificate.inode_generation
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }

        let credits = delalloc_append_batch_journal_credits(reservations.len())?;
        if !self.transaction_credits_fit(credits)? {
            return Err(Ext4Error::new(ErrCode::E2BIG));
        }

        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks
            [self.inode_mutation_lock_index(first_certificate.inode_id)]
        .lock();
        let mut inode = self.read_inode(first_certificate.inode_id)?;
        if !inode.inode.is_file()
            || !inode.inode.uses_extents()
            || inode.inode.generation() != first_certificate.inode_generation
            || inode.inode.size() != first_certificate.expected_durable_eof_before
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let on_disk_eof = inode.inode.size();
        let mut transaction = self.transaction_start(credits)?;
        let mut data_consumptions = Vec::new();
        let mut metadata_consumptions = Vec::new();
        let mut metadata_pool_indices = Vec::new();
        let mut data_allocations = Vec::new();
        data_consumptions
            .try_reserve_exact(reservations.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        metadata_consumptions
            .try_reserve_exact(pool.leases.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        metadata_pool_indices
            .try_reserve_exact(pool.leases.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        data_allocations
            .try_reserve_exact(reservations.len())
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;

        let staged = (|| -> Result<()> {
            // Query the durable tree before staging an append that may replace the
            // inline root with transaction-private external nodes.  The tail write
            // is beyond durable EOF, so it is safe if the transaction later aborts;
            // on success it is covered by the batch's single data flush below.
            self.write_delalloc_append_eof_tail(&inode, on_disk_eof, first_certificate.offset)?;

            for (certificate, reservation) in certificates.iter().zip(reservations.iter_mut()) {
                let start_lblock = u32::try_from(certificate.offset / BLOCK_SIZE)
                    .map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
                let extent_plan =
                    self.transaction_right_spine_append_plan(&transaction, &inode, start_lblock)?;
                let (allocation, data_consumption) = self.transaction_alloc_delalloc_range(
                    &mut transaction,
                    certificate.inode_id,
                    extent_plan.preferred_first,
                    false,
                    1,
                    &reservation.lease,
                )?;
                data_consumptions.push(data_consumption);
                let metadata_blocks = if extent_plan.can_merge(allocation.first, 1) {
                    0
                } else {
                    extent_plan.new_nodes()
                };
                let mut metadata_homes = Vec::new();
                metadata_homes
                    .try_reserve_exact(metadata_blocks)
                    .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
                for _ in 0..metadata_blocks {
                    let pool_index = pool
                        .leases
                        .iter()
                        .enumerate()
                        .find_map(|(index, lease)| {
                            (lease.active
                                && lease.metadata_blocks != 0
                                && !metadata_pool_indices.contains(&index))
                            .then_some(index)
                        })
                        .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
                    let (metadata_allocation, consumption) = self
                        .transaction_alloc_delalloc_metadata_block(
                            &mut transaction,
                            certificate.inode_id,
                            Some(allocation.first),
                            &pool.leases[pool_index],
                        )?;
                    metadata_pool_indices.push(pool_index);
                    metadata_homes.push(metadata_allocation.first);
                    metadata_consumptions.push(consumption);
                }
                self.stage_journaled_right_spine_append(
                    &mut transaction,
                    &mut inode,
                    &extent_plan,
                    &metadata_homes,
                    allocation.first,
                    1,
                )?;
                data_allocations.push(allocation);
            }

            let mut run_start = 0usize;
            while run_start < data_allocations.len() {
                let mut run_end = run_start + 1;
                while run_end < data_allocations.len()
                    && data_allocations[run_end].first
                        == data_allocations[run_end - 1]
                            .first
                            .checked_add(1)
                            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?
                {
                    run_end += 1;
                }
                self.block_device.write_blocks(
                    data_allocations[run_start].first,
                    initialized_blocks[run_start..run_end].as_flattened(),
                )?;
                run_start = run_end;
            }
            self.block_device.flush()?;

            let last_publication = publications
                .last()
                .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
            inode.inode.set_size(last_publication.durable_eof);
            if let Some(mtime) = last_publication.mtime {
                inode.inode.set_mtime(mtime);
            }
            if let Some(ctime) = last_publication.ctime {
                inode.inode.set_ctime(ctime);
            }
            self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)
        })();

        if let Err(error) = staged {
            transaction.abort();
            let cleanup = self
                .rollback_delalloc_consumption_vectors(data_consumptions, metadata_consumptions);
            if cleanup.is_err() {
                self.poison(ErrCode::EIO);
                for reservation in reservations.iter_mut() {
                    reservation.lease.deactivate();
                }
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            return Err(error);
        }

        match transaction.commit(self.block_device.as_ref(), self) {
            Ok(()) => {
                let mut failure = None;
                for (reservation, consumption) in
                    reservations.iter_mut().zip(data_consumptions.iter_mut())
                {
                    if let Err(error) = self.finalize_delalloc_append_block_with_pool(
                        &mut reservation.lease,
                        consumption,
                        None,
                    ) {
                        failure.get_or_insert(error);
                    }
                }
                for (pool_index, consumption) in metadata_pool_indices
                    .iter()
                    .copied()
                    .zip(metadata_consumptions.drain(..))
                {
                    if let Err(error) =
                        self.commit_delalloc_allocation(consumption, &mut pool.leases[pool_index])
                    {
                        failure.get_or_insert(error);
                    }
                }
                for index in metadata_pool_indices.into_iter().rev() {
                    if index < pool.leases.len() && !pool.leases[index].active {
                        drop(pool.leases.swap_remove(index));
                    }
                }
                if let Some(error) = failure {
                    self.poison(ErrCode::EIO);
                    return Err(error);
                }
                Ok(())
            }
            Err(error)
                if !error.poisoned
                    && error.failure == super::journal_transaction::CommitFailure::BeforeCommit =>
            {
                let cleanup = self.rollback_delalloc_consumption_vectors(
                    data_consumptions,
                    metadata_consumptions,
                );
                if cleanup.is_err() {
                    self.poison(ErrCode::EIO);
                    for reservation in reservations.iter_mut() {
                        reservation.lease.deactivate();
                    }
                    return Err(Ext4Error::new(ErrCode::EIO));
                }
                Err(error.error)
            }
            Err(error) => {
                self.poison(ErrCode::EIO);
                for reservation in reservations.iter_mut() {
                    reservation.lease.deactivate();
                }
                for consumption in data_consumptions.iter_mut() {
                    consumption.resolve();
                }
                for consumption in metadata_consumptions.iter_mut() {
                    consumption.resolve();
                }
                Err(error.error)
            }
        }
    }

    pub fn submit_delalloc_append_batch_authorized_with_pool(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservations: &mut [&mut DelallocAppendBlockReservation],
        publications: &[DelallocAppendBlockPublication<'_>],
        pool: &mut DelallocExtentNodePool,
    ) -> DelallocAppendBlockSubmitOutcome {
        if let Err(error) = self.validate_delalloc_append_mapper_authority(authority) {
            let mount_generation = self.delalloc_mount_generation();
            if authority.mount_generation == mount_generation
                && reservations
                    .iter()
                    .all(|reservation| reservation.lease.belongs_to_mount(mount_generation))
            {
                self.poison(ErrCode::EIO);
                for reservation in reservations.iter_mut() {
                    reservation.lease.deactivate();
                }
                return DelallocAppendBlockSubmitOutcome::Terminal(error.code());
            }
            return DelallocAppendBlockSubmitOutcome::RetryableNotPublished(error.code());
        }
        let mount_generation = self.delalloc_mount_generation();
        if reservations
            .iter()
            .any(|reservation| !reservation.lease.belongs_to_mount(mount_generation))
        {
            return DelallocAppendBlockSubmitOutcome::RetryableNotPublished(ErrCode::EINVAL);
        }
        let outcome = self.submit_delalloc_append_batch_inner(reservations, publications, pool);
        let result = match outcome {
            Ok(()) => DelallocAppendBlockSubmitOutcome::Completed,
            Err(error)
                if reservations
                    .iter()
                    .any(|reservation| !reservation.lease.active)
                    || self.poisoned.lock().is_some() =>
            {
                for reservation in reservations.iter_mut() {
                    if reservation.lease.active {
                        reservation.lease.deactivate();
                    }
                }
                DelallocAppendBlockSubmitOutcome::Terminal(error.code())
            }
            Err(error) if is_delalloc_contract_error(error.code()) => {
                self.poison(ErrCode::EIO);
                for reservation in reservations.iter_mut() {
                    reservation.lease.deactivate();
                }
                DelallocAppendBlockSubmitOutcome::Terminal(ErrCode::EIO)
            }
            Err(error) => DelallocAppendBlockSubmitOutcome::RetryableNotPublished(error.code()),
        };
        if !matches!(
            result,
            DelallocAppendBlockSubmitOutcome::RetryableNotPublished(_)
        ) {
            for reservation in reservations.iter_mut() {
                reservation.pool_checkpoint = None;
            }
        }
        result
    }

    /// Submit a production reservation through its issuing mount authority.
    pub fn submit_delalloc_append_block_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservation: &mut DelallocAppendBlockReservation,
        payload: &[u8],
        durable_eof: u64,
        mtime: Option<u32>,
    ) -> DelallocAppendBlockSubmitOutcome {
        self.submit_delalloc_append_block_authorized_with_times(
            authority,
            reservation,
            payload,
            durable_eof,
            mtime,
            None,
        )
    }

    /// Production variant which atomically publishes both Linux write
    /// timestamps with the durable EOF.
    pub fn submit_delalloc_append_block_authorized_with_times(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservation: &mut DelallocAppendBlockReservation,
        payload: &[u8],
        durable_eof: u64,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> DelallocAppendBlockSubmitOutcome {
        self.submit_delalloc_append_block_authorized_with_pool(
            authority,
            reservation,
            DelallocAppendBlockPublication {
                payload,
                durable_eof,
                mtime,
                ctime,
            },
            None,
        )
    }

    pub fn submit_delalloc_append_block_authorized_with_pool(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservation: &mut DelallocAppendBlockReservation,
        publication: DelallocAppendBlockPublication<'_>,
        pool: Option<&mut DelallocExtentNodePool>,
    ) -> DelallocAppendBlockSubmitOutcome {
        if let Err(error) = self.validate_delalloc_append_mapper_authority(authority) {
            // A capability issued by this exact mount must not remain live
            // when validation fails only because another writer has already
            // fail-stopped the mount.  The VFS token may now be destroyed, so
            // consume its ownership without restoring poisoned ledger
            // capacity.  A foreign authority is intentionally left alone.
            if authority.mount_generation == self.delalloc_mount_generation()
                && reservation
                    .lease
                    .belongs_to_mount(self.delalloc_mount_generation())
                && reservation.lease.active
            {
                self.poison(ErrCode::EIO);
                if self
                    .terminalize_delalloc_append_block_authorized_after_fail_stop(
                        authority,
                        reservation,
                    )
                    .is_ok()
                {
                    return DelallocAppendBlockSubmitOutcome::Terminal(error.code());
                }
            }
            return DelallocAppendBlockSubmitOutcome::RetryableNotPublished(error.code());
        }
        self.submit_delalloc_append_block_capability_inner(
            reservation,
            publication,
            pool,
            false,
            true,
        )
    }

    #[cfg(any(test, feature = "test-api"))]
    pub fn submit_delalloc_append_block_capability(
        &self,
        reservation: &mut DelallocAppendBlockReservation,
        payload: &[u8],
        durable_eof: u64,
        mtime: Option<u32>,
    ) -> DelallocAppendBlockSubmitOutcome {
        let payload = reservation
            .lease
            .append_block_certificate()
            .and_then(|certificate| {
                durable_eof
                    .checked_sub(certificate.offset as u64)
                    .and_then(|len| usize::try_from(len).ok())
            })
            .filter(|visible| *visible <= payload.len() && payload.len() == BLOCK_SIZE)
            .map_or(payload, |visible| &payload[..visible]);
        self.submit_delalloc_append_block_capability_inner(
            reservation,
            DelallocAppendBlockPublication {
                payload,
                durable_eof,
                mtime,
                ctime: None,
            },
            None,
            false,
            false,
        )
    }

    /// Release one unmaterialised append capability.  A successful release
    /// consumes exactly the opaque lease it contains; callers cannot release
    /// a different generic reservation by mistake.  Once the mapper has
    /// entered a journal publication attempt this operation is intentionally
    /// rejected by the underlying ledger/lease state.
    fn release_delalloc_append_block_capability_inner(
        &self,
        reservation: &mut DelallocAppendBlockReservation,
    ) -> Result<()> {
        if reservation.pool_checkpoint.is_some() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.release_delalloc_lease_batch(&mut [&mut reservation.lease])
    }

    /// Cancel the newest projected admission before its PageCache dirty
    /// transition is published.  The data lease, pool capacity delta and
    /// right-spine frontier are one atomic lower-layer state transition; the
    /// VFS neither inspects nor reconstructs extent-tree accounting.
    pub fn cancel_projected_delalloc_append_block_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservation: &mut DelallocAppendBlockReservation,
        pool: &mut DelallocExtentNodePool,
    ) -> Result<()> {
        self.validate_delalloc_append_mapper_authority(authority)?;
        let checkpoint = reservation
            .pool_checkpoint
            .as_ref()
            .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
        if pool.inode_id
            != reservation
                .lease
                .append_block_certificate()
                .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?
                .inode_id
            || pool.projection != checkpoint.after
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }

        let mut selected: Vec<usize> = pool
            .leases
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, lease)| lease.active.then_some(index))
            .take(checkpoint.added_metadata_leases)
            .collect();
        if selected.len() != checkpoint.added_metadata_leases {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        selected.sort_unstable();
        let mut leases: Vec<&mut DelallocLease> = Vec::with_capacity(selected.len() + 1);
        leases.push(&mut reservation.lease);
        let mut remaining = pool.leases.as_mut_slice();
        let mut base = 0usize;
        for index in selected.iter().copied() {
            let relative = index
                .checked_sub(base)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            let (_, tail) = remaining.split_at_mut(relative);
            let (lease, rest) = tail
                .split_first_mut()
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            leases.push(lease);
            remaining = rest;
            base = index
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        }
        self.release_delalloc_lease_batch(&mut leases)?;
        drop(leases);

        for index in selected.into_iter().rev() {
            drop(pool.leases.swap_remove(index));
        }
        let checkpoint = reservation
            .pool_checkpoint
            .take()
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        pool.projection = checkpoint.before;
        Ok(())
    }

    pub fn release_delalloc_append_block_authorized(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservation: &mut DelallocAppendBlockReservation,
    ) -> Result<()> {
        self.validate_delalloc_append_mapper_authority(authority)?;
        self.release_delalloc_append_block_capability_inner(reservation)
    }

    #[cfg(any(test, feature = "test-api"))]
    pub fn release_delalloc_append_block_capability(
        &self,
        reservation: &mut DelallocAppendBlockReservation,
    ) -> Result<()> {
        self.release_delalloc_append_block_capability_inner(reservation)
    }

    /// Terminalise an append capability only after this mount has fail-stopped.
    /// This mirrors the generic lease operation while preserving the typed
    /// ownership boundary for the VFS token finaliser.
    fn abandon_delalloc_append_block_capability_after_fail_stop_inner(
        &self,
        reservation: &mut DelallocAppendBlockReservation,
    ) -> Result<()> {
        self.abandon_delalloc_lease_after_fail_stop(&mut reservation.lease)
    }

    pub fn abandon_delalloc_append_block_authorized_after_fail_stop(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservation: &mut DelallocAppendBlockReservation,
    ) -> Result<()> {
        if authority.mount_generation != self.delalloc_mount_generation()
            || !reservation
                .lease
                .belongs_to_mount(self.delalloc_mount_generation())
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.abandon_delalloc_append_block_capability_after_fail_stop_inner(reservation)
    }

    /// Destroy a capability proven to belong to this mount after fail-stop.
    /// A foreign reservation is rejected without mutation, so `Terminal`
    /// never lies about ownership.
    pub fn terminalize_delalloc_append_block_authorized_after_fail_stop(
        &self,
        authority: &DelallocAppendMapperAuthority,
        reservation: &mut DelallocAppendBlockReservation,
    ) -> Result<()> {
        if authority.mount_generation != self.delalloc_mount_generation()
            || !reservation
                .lease
                .belongs_to_mount(self.delalloc_mount_generation())
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.poison(ErrCode::EIO);
        if reservation.lease.active {
            reservation.lease.deactivate();
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-api"))]
    pub fn abandon_delalloc_append_block_capability_after_fail_stop(
        &self,
        reservation: &mut DelallocAppendBlockReservation,
    ) -> Result<()> {
        self.abandon_delalloc_append_block_capability_after_fail_stop_inner(reservation)
    }

    /// Shared outcome classifier for the typed capability and the host-only
    /// raw compatibility facade.  Keeping the classifier in one place avoids
    /// a divergent retry/terminal decision at the production boundary.
    fn submit_delalloc_append_block_with_lease(
        &self,
        id: InodeId,
        request: DelallocAppendBlockWriteback<'_>,
        lease: &mut DelallocLease,
        pool: Option<&mut DelallocExtentNodePool>,
        policy: DelallocAppendSubmitPolicy,
    ) -> DelallocAppendBlockSubmitOutcome {
        let DelallocAppendSubmitPolicy {
            strict_aligned,
            journal_credits_bound,
            terminal_on_contract_error,
        } = policy;
        // Preserve the linear outcome contract before inspecting provenance:
        // a terminal lease can never be reported as retryable, even when a
        // caller incorrectly presents it to a foreign mount.
        if !lease.active {
            return DelallocAppendBlockSubmitOutcome::Terminal(ErrCode::EINVAL);
        }
        // Validate provenance before `ensure_mutable()`: a poisoned foreign
        // filesystem must never terminalise a live capability owned by the
        // lease's source mount. The source owner can still explicitly release
        // or abandon it according to its own lifecycle.
        if !lease.belongs_to_mount(self.delalloc_mount_generation()) {
            return DelallocAppendBlockSubmitOutcome::RetryableNotPublished(ErrCode::EINVAL);
        }
        match self.submit_delalloc_append_block_inner(
            id,
            request,
            lease,
            pool,
            strict_aligned,
            journal_credits_bound,
        ) {
            Ok(()) => DelallocAppendBlockSubmitOutcome::Completed,
            // The mapper can terminalise a lease itself after a commit
            // failure. Re-check after the call as well as before it so that
            // path never falls through to the pre-existing-fail-stop
            // abandonment branch.
            Err(error) if !lease.active => DelallocAppendBlockSubmitOutcome::Terminal(error.code()),
            // `ensure_mutable()` can reject this request because a different
            // writer already fail-stopped the mount. Returning that active
            // lease as retryable would make a future queue/token own a
            // capability which can no longer be released through the normal
            // mutable path. Terminalise it without restoring ledger capacity:
            // this mount must never allocate again, and recovery owns any
            // reserved state after remount.
            Err(error) if self.poisoned.lock().is_some() => {
                lease.deactivate();
                DelallocAppendBlockSubmitOutcome::Terminal(error.code())
            }
            // Production admission proves that its private journal-credit
            // bound fits before Dirty publication. Reaching E2BIG here is an
            // internal contract violation, never transient contention.
            Err(error)
                if terminal_on_contract_error && is_delalloc_contract_error(error.code()) =>
            {
                self.poison(ErrCode::EIO);
                lease.deactivate();
                DelallocAppendBlockSubmitOutcome::Terminal(ErrCode::EIO)
            }
            Err(error) if strict_aligned && is_delalloc_contract_error(error.code()) => {
                self.poison(ErrCode::EIO);
                lease.deactivate();
                DelallocAppendBlockSubmitOutcome::Terminal(ErrCode::EIO)
            }
            Err(error) if !strict_aligned && is_delalloc_contract_error(error.code()) => {
                // Host/test compatibility callers retain the old explicit
                // release contract. Production callers take the terminal
                // branch above for deterministic contract failures.
                DelallocAppendBlockSubmitOutcome::RetryableNotPublished(ErrCode::EAGAIN)
            }
            Err(error) => DelallocAppendBlockSubmitOutcome::RetryableNotPublished(error.code()),
        }
    }

    /// Host-recovery compatibility facade.  The typed test facade above
    /// fixes the inode and offset at reservation time; neither entry is a
    /// normal DragonOS VFS writeback API before the future token bridge exists.
    #[cfg(any(test, feature = "test-api"))]
    pub fn submit_delalloc_append_block(
        &self,
        id: InodeId,
        request: DelallocAppendBlockWriteback<'_>,
        lease: &mut DelallocLease,
    ) -> DelallocAppendBlockSubmitOutcome {
        let visible = request
            .durable_eof
            .checked_sub(request.offset as u64)
            .and_then(|len| usize::try_from(len).ok());
        let payload = visible
            .filter(|visible| {
                *visible <= request.payload.len() && request.payload.len() == BLOCK_SIZE
            })
            .map_or(request.payload, |visible| &request.payload[..visible]);
        let request = DelallocAppendBlockWriteback { payload, ..request };
        self.submit_delalloc_append_block_with_lease(
            id,
            request,
            lease,
            None,
            DelallocAppendSubmitPolicy {
                strict_aligned: false,
                journal_credits_bound: None,
                terminal_on_contract_error: false,
            },
        )
    }

    /// Host-recovery compatibility wrapper for the pre-contract probe API.
    /// Test callers which need publication-aware classification use the typed
    /// capability facade; this wrapper preserves the historical `Result` API.
    #[cfg(any(test, feature = "test-api"))]
    pub fn writeback_delalloc_append_block(
        &self,
        id: InodeId,
        request: DelallocAppendBlockWriteback<'_>,
        lease: &mut DelallocLease,
    ) -> Result<()> {
        match self.submit_delalloc_append_block(id, request, lease) {
            DelallocAppendBlockSubmitOutcome::Completed => Ok(()),
            DelallocAppendBlockSubmitOutcome::RetryableNotPublished(error)
            | DelallocAppendBlockSubmitOutcome::Terminal(error) => Err(Ext4Error::new(error)),
        }
    }

    /// Build the deliberately narrow first delayed-allocation map shape.
    ///
    /// Aggregate reservation alone cannot promise an arbitrary contiguous
    /// range on a fragmented filesystem.  The first production mapper
    /// primitive therefore accepts exactly one filesystem block and refuses
    /// every tree split: after a successful reservation, searching all block
    /// groups is sufficient to find one data block, while no additional
    /// metadata allocation can fail after the foreground write succeeded.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    fn delalloc_single_block_append_plan(
        &self,
        inode: &InodeRef,
        offset: usize,
    ) -> Result<TransactionalRangePlan> {
        if !offset.is_multiple_of(BLOCK_SIZE) || inode.inode.size() != offset as u64 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let start_lblock =
            u32::try_from(offset / BLOCK_SIZE).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        let shape = self
            .direct_append_shape(inode, start_lblock, 1)?
            .ok_or_else(|| Ext4Error::new(ErrCode::ENOTSUP))?;
        // The test primitive has no metadata reservation or VFS certificate
        // for an extent-tree split. A full leaf that can accept the append
        // only by physically merging also needs an exact adjacent block,
        // which an aggregate data reservation cannot promise. Reject all of
        // those shapes before consuming the lease.
        if shape.requires_merge || shape.requires_root_split || shape.requires_leaf_split {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        Ok(TransactionalRangePlan {
            start_lblock,
            count: 1,
            preferred_first: shape.preferred_first,
            // A non-adjacent physical block may form a new in-inode/leaf
            // extent without allocating metadata. Requiring adjacency here
            // would turn a valid aggregate reservation into late ENOSPC.
            requires_merge: false,
            requires_root_split: false,
            requires_leaf_split: false,
            leaf_home: shape.leaf_home,
        })
    }

    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    fn abort_delalloc_mapping_transaction(
        &self,
        transaction: super::journal_transaction::Transaction<'_>,
        consumption: super::alloc::DelallocConsumption,
    ) -> Result<()> {
        transaction.abort();
        self.rollback_delalloc_allocation(consumption)
    }

    /// The host-only raw mapper leaves a linked tail between map and payload
    /// completion.  Its generic publishers must not publish an unrelated
    /// size or overwrite that payload.  This scan is deliberately absent
    /// from normal builds: production has no raw receipt and must eventually
    /// use the VFS queue/lifecycle/drain protocol, not add an orphan-chain
    /// walk to every ordinary write while a zero-link orphan is pending.
    #[cfg(any(test, feature = "test-api"))]
    fn reject_external_linked_tail_mutation(&self, inode: &InodeRef) -> Result<()> {
        if self.legacy_orphan_membership(inode)? == LegacyOrphanMembership::LinkedTail {
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }
        Ok(())
    }

    #[cfg(not(any(test, feature = "test-api")))]
    #[inline]
    fn reject_external_linked_tail_mutation(&self, _inode: &InodeRef) -> Result<()> {
        Ok(())
    }

    #[cfg(any(test, feature = "test-api"))]
    fn fail_mapped_delalloc_writeback<T>(
        &self,
        receipt: &mut DelallocMappedWriteback,
        error: Ext4Error,
    ) -> Result<T> {
        // A transaction-owner collision is the only retryable outcome after
        // the data block exists. The future PageCache token converts it to a
        // progress ticket while retaining this exact receipt. Any other
        // failure is either corruption or an unpublishable metadata state;
        // leaving a live receipt for a generic error path would make Drop
        // fail-stop without a recovery owner, so poison the mount and hand
        // the tail to orphan recovery instead.
        if error.code() == ErrCode::EAGAIN {
            return Err(error);
        }
        receipt.deactivate();
        self.poison(ErrCode::EIO);
        Err(error)
    }

    /// Materialise one previously reserved, full-block append for the host
    /// recovery harness.
    ///
    /// This deliberately is not a production mapper API. It proves the
    /// ledger -> bitmap -> zero -> linked-orphan -> extent publication
    /// boundary while the VFS token, queue claim, lifecycle lease and
    /// truncate drain are still being implemented. The normal write path
    /// cannot name either this method or its receipt.
    ///
    /// On success the returned receipt owns the mapped-but-not-yet-visible
    /// tail. Call [`Self::test_writeback_delalloc_mapped_block`] with the
    /// exact block payload to submit data, persist EOF and remove the linked
    /// orphan.
    /// If mapping publication is uncertain the filesystem is poisoned and the
    /// lease is finalised rather than being returned to free space.
    #[cfg(any(test, feature = "test-api"))]
    pub fn test_map_delalloc_reserved_block_append(
        &self,
        id: InodeId,
        offset: usize,
        lease: &mut DelallocLease,
    ) -> Result<DelallocMappedWriteback> {
        self.ensure_mutable()?;
        if !self.uses_journal() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        if !lease.active || lease.data_blocks != 1 || lease.metadata_blocks != 0 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let end = offset
            .checked_add(BLOCK_SIZE)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        let end = u64::try_from(end).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;

        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if !inode.inode.is_file() || !inode.inode.uses_extents() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        // A linked tail is an already published mapper receipt, not an
        // unsupported extent shape.  Report the queue-head dependency before
        // inspecting append geometry so a future PageCache backend can defer
        // without allocating or misclassifying the follower as a permanent
        // compatibility fallback.
        match self.legacy_orphan_membership(&inode)? {
            LegacyOrphanMembership::Absent => {}
            LegacyOrphanMembership::LinkedTail => return Err(Ext4Error::new(ErrCode::EAGAIN)),
            LegacyOrphanMembership::ZeroLink => return Err(Ext4Error::new(ErrCode::EIO)),
        }
        // Before the production orphan-role index exists, the recovery
        // primitive permits exactly one active orphan entry per mount. This
        // keeps its linear membership walk out of a fake multi-token hot
        // path and makes the missing production index explicit.
        if self.read_super_block_cached().last_orphan() != 0 {
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }
        let plan = self.delalloc_single_block_append_plan(&inode, offset)?;
        // All allocation which can fail independently of the transaction is
        // complete before a lease debit exists. In particular, ENOMEM here
        // must not leave a staged bitmap image or unresolved consumption.
        let mut zeros = Vec::new();
        zeros
            .try_reserve_exact(BLOCK_SIZE)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        zeros.resize(BLOCK_SIZE, 0);
        let credits = if plan.leaf_home.is_some() { 5 } else { 4 };
        let mut transaction = self.transaction_start(credits)?;
        let (allocation, consumption) = match self.transaction_alloc_delalloc_range(
            &mut transaction,
            id,
            plan.preferred_first,
            plan.requires_merge,
            plan.count,
            lease,
        ) {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };

        if let Err(error) = self.initialize_allocated_range(allocation.first, 1, &zeros) {
            self.abort_delalloc_mapping_transaction(transaction, consumption)
                .expect("delalloc allocation rollback after zero failure");
            return Err(error);
        }

        let mut sb = match self.transaction_read_super_block(&transaction) {
            Ok(sb) => sb,
            Err(error) => {
                self.abort_delalloc_mapping_transaction(transaction, consumption)
                    .expect("delalloc allocation rollback after superblock read failure");
                return Err(error);
            }
        };
        let enrolled =
            match self.transaction_ensure_linked_tail_orphan(&mut transaction, &mut inode, &mut sb)
            {
                Ok(enrolled) => enrolled,
                Err(error) => {
                    self.abort_delalloc_mapping_transaction(transaction, consumption)
                        .expect("delalloc allocation rollback after orphan enrollment failure");
                    return Err(error);
                }
            };
        // A live linked tail belongs to an earlier mapped receipt. Allowing a
        // second raw mapper call would make one orphan removal incorrectly
        // cover multiple uncommitted tails. The future queue may defer on its
        // head, but this primitive must not silently merge those lifetimes.
        if !enrolled {
            self.abort_delalloc_mapping_transaction(transaction, consumption)
                .expect("delalloc allocation rollback after duplicate tail enrollment");
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }
        if let Err(error) = self.stage_journaled_append_extent(
            &mut transaction,
            &mut inode,
            super::extent::JournaledAppendExtent {
                leaf_home: plan.leaf_home,
                root_split_leaf_home: None,
                leaf_split_new_home: None,
                start_lblock: plan.start_lblock,
                start_pblock: allocation.first,
                count: 1,
            },
        ) {
            self.abort_delalloc_mapping_transaction(transaction, consumption)
                .expect("delalloc allocation rollback after extent staging failure");
            return Err(error);
        }
        if let Err(error) = self.transaction_stage_inode_with_csum(&mut transaction, &mut inode) {
            self.abort_delalloc_mapping_transaction(transaction, consumption)
                .expect("delalloc allocation rollback after inode staging failure");
            return Err(error);
        }

        match transaction.commit(self.block_device.as_ref(), self) {
            Ok(()) => {
                self.commit_delalloc_allocation(consumption, lease)
                    .expect("published delayed mapping must finalise its exact lease");
                Ok(DelallocMappedWriteback {
                    mount_generation: self.delalloc_mount_generation(),
                    inode_id: id,
                    inode_generation: inode.inode.generation(),
                    offset,
                    end,
                    active: true,
                    _not_send: core::marker::PhantomData,
                })
            }
            Err(error) => {
                if error.failure == super::journal_transaction::CommitFailure::BeforeCommit {
                    // No commit record exists. Restore the debit and drop the
                    // whole claim while still in the transactional exclusion
                    // domain. If another fail-stop wins the ledger race, do
                    // not restore capacity: terminalise the active lease.
                    self.rollback_delalloc_allocation(consumption)
                        .expect("unpublished delayed mapping must roll back its debit");
                    if self
                        .release_delalloc_lease_after_transaction_abort(lease)
                        .is_err()
                    {
                        self.poison(ErrCode::EIO);
                        self.abandon_delalloc_lease_after_fail_stop(lease)
                            .expect("poisoned unpublished delayed mapping must abandon its lease");
                    }
                } else {
                    // The log may be replayed. Preserve the debit/bitmap
                    // ownership and let linked-orphan recovery clean the tail
                    // after remount; reuse on this poisoned mount is unsafe.
                    self.commit_delalloc_allocation(consumption, lease)
                        .expect("uncertain delayed mapping must finalise its exact lease");
                }
                self.poison(ErrCode::EIO);
                Err(error.error)
            }
        }
    }

    /// Submit the exact payload for a mapped delayed block, then make its EOF
    /// durable and remove the linked truncate orphan in one journal
    /// transaction.  A payload I/O failure leaves the receipt active for a
    /// future retry; a metadata publication failure poisons the mount and
    /// deactivates the receipt because recovery owns the remaining tail.
    #[cfg(any(test, feature = "test-api"))]
    pub fn test_writeback_delalloc_mapped_block(
        &self,
        receipt: &mut DelallocMappedWriteback,
        data: &[u8],
        mtime: Option<u32>,
    ) -> Result<()> {
        if !receipt.active || data.len() != BLOCK_SIZE {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if self.delalloc_mount_generation() != receipt.mount_generation {
            // Do not poison an unrelated mount or consume a receipt which
            // still belongs to its source filesystem.
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if let Err(error) = self.ensure_mutable() {
            // A matching mount is already fail-stopped.  The reservation is
            // intentionally not released, but this in-memory capability must
            // become terminal so teardown cannot turn a prior I/O error into
            // a Drop assertion.
            receipt.deactivate();
            return Err(error);
        }
        match self.write_delalloc_mapped_data(receipt, data) {
            Ok(()) => {}
            Err(DelallocMappedDataFailure::Retryable(error)) => return Err(error),
            Err(DelallocMappedDataFailure::Fatal(error)) => {
                return self.fail_mapped_delalloc_writeback(receipt, error)
            }
        }

        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(receipt.inode_id)].lock();
        let mut inode = match self.read_inode(receipt.inode_id) {
            Ok(inode) => inode,
            Err(error) => return self.fail_mapped_delalloc_writeback(receipt, error),
        };
        let membership = match self.legacy_orphan_membership(&inode) {
            Ok(membership) => membership,
            Err(error) => return self.fail_mapped_delalloc_writeback(receipt, error),
        };
        if !inode.inode.is_file()
            || inode.inode.generation() != receipt.inode_generation
            || inode.inode.size() != receipt.offset as u64
            || membership != LegacyOrphanMembership::LinkedTail
        {
            return self.fail_mapped_delalloc_writeback(receipt, Ext4Error::new(ErrCode::EIO));
        }
        inode.inode.set_size(receipt.end);
        if let Some(mtime) = mtime {
            inode.inode.set_mtime(mtime);
        }
        // A non-head member may require a predecessor inode-table image;
        // head and inode images are deduplicated by Transaction, so three
        // credits bound both forms without relying on test ordering.
        let mut transaction = match self.transaction_start(3) {
            Ok(transaction) => transaction,
            Err(error) => return self.fail_mapped_delalloc_writeback(receipt, error),
        };
        let mut sb = match self.transaction_read_super_block(&transaction) {
            Ok(sb) => sb,
            Err(error) => {
                transaction.abort();
                return self.fail_mapped_delalloc_writeback(receipt, error);
            }
        };
        if let Err(error) = self.transaction_orphan_del(&mut transaction, &inode, &mut sb) {
            transaction.abort();
            return self.fail_mapped_delalloc_writeback(receipt, error);
        }
        if let Err(error) = self.transaction_stage_inode_with_csum(&mut transaction, &mut inode) {
            transaction.abort();
            return self.fail_mapped_delalloc_writeback(receipt, error);
        }
        match transaction.commit(self.block_device.as_ref(), self) {
            Ok(()) => {
                receipt.deactivate();
                Ok(())
            }
            Err(error) => {
                // Size/orphan publication is now uncertain or the journal
                // core rejected a commit.  In either case this mount cannot
                // safely reuse the receipt; recovery will decide from the
                // durable log and orphan list on the next mount.
                receipt.deactivate();
                self.poison(ErrCode::EIO);
                Err(error.error)
            }
        }
    }

    /// Terminalise a test receipt only after its owning mount has already
    /// fail-stopped.  This does not remove the on-disk linked tail or make the
    /// filesystem writable again; recovery remains the owner of that state.
    #[cfg(any(test, feature = "test-api"))]
    pub fn test_abandon_mapped_delalloc_after_fail_stop(
        &self,
        receipt: &mut DelallocMappedWriteback,
    ) -> Result<()> {
        if !receipt.active
            || self.delalloc_mount_generation() != receipt.mount_generation
            || self.poisoned.lock().is_none()
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        receipt.deactivate();
        Ok(())
    }

    fn initialize_allocated_range(
        &self,
        first: PBlockId,
        count: usize,
        zeros: &[u8],
    ) -> Result<()> {
        let mut done = 0usize;
        while done < count {
            let blocks = min(DIRECT_RANGE_ZERO_CHUNK_BLOCKS, count - done);
            let pblock = first
                .checked_add(done as PBlockId)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            self.block_device
                .write_blocks(pblock, &zeros[..blocks * BLOCK_SIZE])?;
            self.prepare_stats.record_zero_io();
            done += blocks;
        }
        self.block_device.flush()
    }

    fn transactional_range_plan(
        &self,
        inode: &InodeRef,
        offset: usize,
        len: usize,
    ) -> Result<Option<TransactionalRangePlan>> {
        if len == 0 {
            return Ok(None);
        }
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        let mut start_lblock =
            u32::try_from(offset / BLOCK_SIZE).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        let end_lblock =
            u32::try_from((end - 1) / BLOCK_SIZE).map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        let original_count = end_lblock
            .checked_sub(start_lblock)
            .and_then(|blocks| blocks.checked_add(1))
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        if start_lblock.checked_add(original_count).is_none()
            || (original_count as usize) > DIRECT_RANGE_MAX_BLOCKS
        {
            return Ok(None);
        }

        // Sequential userspace writes need not be block aligned.  A 64 KiB
        // circular buffer commonly starts the next write in the final block
        // of the preceding write.  Trim that already-mapped prefix so the
        // remaining append can still use one range transaction instead of
        // falling back to one allocation transaction per 4 KiB block.
        while start_lblock <= end_lblock {
            match self.extent_query(inode, start_lblock) {
                Ok(_) => start_lblock += 1,
                Err(error) if error.code() == ErrCode::ENOENT => break,
                Err(error) => return Err(error),
            }
        }
        if start_lblock > end_lblock {
            return Ok(None);
        }
        let count = end_lblock - start_lblock + 1;
        if (count as usize) < DIRECT_RANGE_MIN_BLOCKS {
            return Ok(None);
        }

        let persistent_blocks = inode
            .inode
            .size()
            .checked_add(BLOCK_SIZE as u64 - 1)
            .map(|size| size / BLOCK_SIZE as u64)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        if (start_lblock as u64) < persistent_blocks {
            return Ok(None);
        }
        let Some(shape) = self.direct_append_shape(inode, start_lblock, count)? else {
            return Ok(None);
        };
        Ok(Some(TransactionalRangePlan {
            start_lblock,
            count,
            preferred_first: shape.preferred_first,
            requires_merge: shape.requires_merge,
            requires_root_split: shape.requires_root_split,
            requires_leaf_split: shape.requires_leaf_split,
            leaf_home: shape.leaf_home,
        }))
    }

    fn try_prepare_transactional_range(
        &self,
        inode: &mut InodeRef,
        offset: usize,
        len: usize,
    ) -> Result<TransactionalRangePrepare> {
        if len == 0 {
            return Ok(TransactionalRangePrepare::Handled);
        }
        let Some(plan) = self.transactional_range_plan(inode, offset, len)? else {
            return Ok(TransactionalRangePrepare::Unsupported);
        };

        let zero_bytes = DIRECT_RANGE_ZERO_CHUNK_BLOCKS * BLOCK_SIZE;
        let mut zeros = Vec::new();
        if zeros.try_reserve_exact(zero_bytes).is_err() {
            return Ok(TransactionalRangePrepare::Unsupported);
        }
        zeros.resize(zero_bytes, 0);

        let journaled = self.uses_journal();
        let credits = if journaled && plan.requires_leaf_split {
            // Two allocation groups can contribute two bitmap/descriptor
            // pairs plus one superblock; the new leaf and inode root add two
            // more metadata images.  Keep one spare credit so this path never
            // silently falls back to the legacy direct leaf split.
            9
        } else if journaled && plan.requires_root_split {
            // Data and the new extent leaf can land in different groups:
            // bitmap + descriptor for each group, one shared superblock, the
            // leaf image and the inode home require at most seven images.
            // Reserve one extra slot so a conservative transaction never
            // falls back to the non-journaled extent split path.
            8
        } else if journaled && plan.leaf_home.is_some() {
            5
        } else {
            4
        };
        let mut transaction = if journaled {
            self.transaction_start(credits)?
        } else {
            self.transaction_start_direct_range(credits)?
        };
        let allocation = match self.transaction_alloc_range(
            &mut transaction,
            inode.id,
            plan.preferred_first,
            plan.requires_merge,
            plan.count,
        ) {
            Ok(allocation) => allocation,
            Err(error) if error.code() == ErrCode::ENOSPC => {
                transaction.abort();
                return Ok(TransactionalRangePrepare::Unsupported);
            }
            Err(error) => return Err(error),
        };

        let root_split_leaf_home = if plan.requires_root_split {
            match self.transaction_alloc_range(
                &mut transaction,
                inode.id,
                Some(allocation.first),
                false,
                1,
            ) {
                Ok(allocation) => Some(allocation.first),
                Err(error) if error.code() == ErrCode::ENOSPC => {
                    transaction.abort();
                    return Ok(TransactionalRangePrepare::Unsupported);
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let leaf_split_new_home = if plan.requires_leaf_split {
            match self.transaction_alloc_range(
                &mut transaction,
                inode.id,
                Some(allocation.first),
                false,
                1,
            ) {
                Ok(allocation) => Some(allocation.first),
                Err(error) if error.code() == ErrCode::ENOSPC => {
                    transaction.abort();
                    return Ok(TransactionalRangePrepare::Unsupported);
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };

        self.prepare_stats.record_call();
        self.prepare_stats.record_requested(plan.count as usize);
        self.prepare_stats
            .record_missing_blocks(plan.count as usize);
        let initialized = self.initialize_allocated_range(
            allocation.first,
            plan.count as usize,
            zeros.as_slice(),
        );
        if let Err(error) = initialized {
            self.prepare_stats.record_failure();
            transaction.abort();
            if error.code() == ErrCode::ENOMEM {
                return Ok(TransactionalRangePrepare::Unsupported);
            }
            return Err(error);
        }

        let stage_result = if journaled {
            self.stage_journaled_append_extent(
                &mut transaction,
                inode,
                super::extent::JournaledAppendExtent {
                    leaf_home: plan.leaf_home,
                    root_split_leaf_home,
                    leaf_split_new_home,
                    start_lblock: plan.start_lblock,
                    start_pblock: allocation.first,
                    count: plan.count,
                },
            )
        } else {
            self.stage_direct_append_extent(inode, plan.start_lblock, allocation.first, plan.count)
        };
        if let Err(error) = stage_result {
            self.prepare_stats.record_failure();
            transaction.abort();
            return Err(error);
        }
        let (inode_home, _) = self.inode_disk_pos(inode.id)?;
        if let Err(error) = self.transaction_stage_inode_with_csum(&mut transaction, inode) {
            self.prepare_stats.record_failure();
            transaction.abort();
            return Err(error);
        }
        self.prepare_stats.record_inode_io();
        let commit_result = if journaled {
            transaction.commit(self.block_device.as_ref(), self)
        } else {
            transaction.commit_direct_range(
                self.block_device.as_ref(),
                self,
                &allocation.allocation_homes,
                inode_home,
            )
        };
        if let Err(error) = commit_result {
            self.prepare_stats.record_failure();
            // Both backends classify failures relative to their publication
            // point.  A failed journal commit also poisons its private core;
            // publish the same fail-stop state through the Ext4 facade.
            if journaled || error.failure != super::journal_transaction::CommitFailure::BeforeCommit
            {
                self.poison(ErrCode::EIO);
            }
            return Err(error.error);
        }
        self.prepare_stats.record_bitmap_io();
        self.prepare_stats.record_gdt_io();
        self.prepare_stats.record_superblock_io();
        self.prepare_stats.record_inode_io();
        Ok(TransactionalRangePrepare::Handled)
    }

    fn xattr_checksum_seed(&self) -> Result<Option<MetadataChecksumSeed>> {
        let sb = self.read_super_block_cached();
        if !sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM) {
            return Ok(None);
        }
        Ok(Some(sb.metadata_checksum_seed()))
    }

    fn verify_xattr_block_checksum(&self, block_id: PBlockId, block: &XattrBlock) -> Result<()> {
        if let Some(seed) = self.xattr_checksum_seed()? {
            if !block.verify_checksum(seed, block_id) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
        }
        Ok(())
    }

    fn update_xattr_block_checksum(
        &self,
        block_id: PBlockId,
        block: &mut XattrBlock,
    ) -> Result<()> {
        if let Some(seed) = self.xattr_checksum_seed()? {
            if !block.update_checksum(seed, block_id) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
        }
        Ok(())
    }

    fn read_extent_or_hole(
        &self,
        file: &InodeRef,
        iblock: LBlockId,
        block_offset: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        match self.extent_query(file, iblock) {
            Ok(fblock) => {
                let block = self.read_block(fblock)?;
                buf.copy_from_slice(block.read_offset(block_offset, buf.len()));
            }
            Err(err) if err.code() == ErrCode::ENOENT => {
                buf.fill(0);
            }
            Err(err) => return Err(err),
        }
        Ok(())
    }

    /// Get file attributes.
    ///
    /// # Params
    ///
    /// * `id` - inode id
    ///
    /// # Return
    ///
    /// A file attribute struct.
    ///
    /// # Error
    ///
    /// `EINVAL` if the inode-table entry is physically free.
    pub fn getattr(&self, id: InodeId) -> Result<FileAttr> {
        let inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }

        Ok(Self::file_attr(&inode))
    }

    fn file_attr(inode: &InodeRef) -> FileAttr {
        // Get device number for device nodes
        let rdev = if inode.inode.is_device() {
            inode.inode.device()
        } else {
            (0, 0)
        };

        FileAttr {
            ino: inode.id,
            size: inode.inode.size(),
            blocks: inode.inode.block_count(),
            atime: inode.inode.atime(),
            mtime: inode.inode.mtime(),
            ctime: inode.inode.ctime(),
            crtime: inode.inode.crtime(),
            ftype: inode.inode.file_type(),
            perm: inode.inode.perm(),
            links: inode.inode.link_count(),
            uid: inode.inode.uid(),
            gid: inode.inode.gid(),
            rdev,
        }
    }

    /// Set file attributes.
    ///
    /// # Params
    ///
    /// * `id` - inode id
    /// * `attr` - attributes to set (wrapped in SetAttr struct)
    ///
    /// # Error
    ///
    /// `EINVAL` if the inode is invalid (mode == 0).
    pub fn setattr(&self, id: InodeId, attr: SetAttr) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        if attr.size.is_some() {
            self.reject_external_linked_tail_mutation(&inode)?;
        }
        if let Some(mode) = attr.mode {
            inode.inode.set_mode(mode);
        }
        if let Some(uid) = attr.uid {
            inode.inode.set_uid(uid);
        }
        if let Some(gid) = attr.gid {
            inode.inode.set_gid(gid);
        }
        if let Some(size) = attr.size {
            inode.inode.set_size(size);
        }
        if let Some(atime) = attr.atime {
            inode.inode.set_atime(atime);
        }
        if let Some(mtime) = attr.mtime {
            inode.inode.set_mtime(mtime);
        }
        if let Some(ctime) = attr.ctime {
            inode.inode.set_ctime(ctime);
        }
        if let Some(crtime) = attr.crtime {
            inode.inode.set_crtime(crtime);
        }
        self.write_inode_with_csum(&mut inode)?;
        Ok(())
    }

    fn recompute_inode_block_count(&self, inode: &mut InodeRef) -> Result<()> {
        let data_blocks = self.extent_all_data_blocks(inode)?.len() as u64;
        let tree_blocks = self.extent_all_tree_blocks(inode)?.len() as u64;
        let sectors_per_block = (BLOCK_SIZE / INODE_BLOCK_SIZE) as u64;
        inode
            .inode
            .set_block_count((data_blocks + tree_blocks) * sectors_per_block);
        Ok(())
    }

    fn ensure_blocks_for_write_range_locked(
        &self,
        inode: &mut InodeRef,
        offset: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.prepare_stats.record_call();
        let result = (|| {
            let end = offset.checked_add(len).ok_or(format_error!(
                ErrCode::EFBIG,
                "write range overflow: offset={} len={}",
                offset,
                len
            ))?;
            let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
            let end_iblock = ((end - 1) / BLOCK_SIZE) as LBlockId;
            let requested_blocks = end_iblock
                .checked_sub(start_iblock)
                .and_then(|blocks| blocks.checked_add(1))
                .and_then(|blocks| usize::try_from(blocks).ok())
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            self.prepare_stats.record_requested(requested_blocks);
            let mut changed = false;
            for iblock in start_iblock..=end_iblock {
                match self.extent_query(inode, iblock) {
                    Ok(_) => {
                        self.prepare_stats.record_mapped();
                    }
                    Err(err) if err.code() == ErrCode::ENOENT => {
                        self.prepare_stats.record_missing();
                        self.extent_query_or_create(inode, iblock, 1)?;
                        self.extent_query(inode, iblock).map_err(|err| {
                            format_error!(
                                ErrCode::EIO,
                                "extent allocation invariant failed: inode {} iblock {} missing after create: {:?}",
                                inode.id,
                                iblock,
                                err
                            )
                        })?;
                        changed = true;
                    }
                    Err(err) => return Err(err),
                }
            }
            if changed {
                self.recompute_inode_block_count(inode)?;
                self.write_inode_with_csum(inode)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.prepare_stats.record_failure();
        }
        result
    }

    /// Ensure extents exist for the bytes that will actually be written.
    pub fn allocate_blocks_for_write_range(
        &self,
        id: InodeId,
        offset: usize,
        len: usize,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        self.reject_external_linked_tail_mutation(&inode)?;
        self.ensure_blocks_for_write_range_locked(&mut inode, offset, len)
    }

    /// Prepare a buffered write by allocating only the written range.
    ///
    /// The caller owns the in-memory visible size used by page-cache writeback
    /// and should call `commit_inode_size()` at fsync/truncate-style sync
    /// boundaries.
    pub fn prepare_buffered_write(
        &self,
        id: InodeId,
        offset: usize,
        len: usize,
        _size: u64,
        _mtime: Option<u32>,
    ) -> Result<()> {
        self.ensure_mutable()?;
        if self.uses_journal() || self.supports_direct_range_stage() {
            // Classify the request under a compatible direct guard first.
            // Unsupported small, overwrite, sparse, and oversized writes can
            // continue through the legacy allocator without ever contending
            // for the exclusive transaction snapshot gate.
            {
                let _metadata_guard = self.lock_direct_metadata_mutation()?;
                let _mutation_guard =
                    self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
                let mut inode = self.read_inode(id)?;
                if inode.inode.mode().bits() == 0 {
                    return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
                }
                self.reject_external_linked_tail_mutation(&inode)?;
                if self
                    .transactional_range_plan(&inode, offset, len)?
                    .is_none()
                {
                    return self.ensure_blocks_for_write_range_locked(&mut inode, offset, len);
                }
            }
            let outcome = {
                let _metadata_guard = self.lock_transactional_metadata_mutation()?;
                let _mutation_guard =
                    self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
                let mut inode = self.read_inode(id)?;
                if inode.inode.mode().bits() == 0 {
                    return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
                }
                self.reject_external_linked_tail_mutation(&inode)?;
                self.try_prepare_transactional_range(&mut inode, offset, len)?
            };
            if matches!(outcome, TransactionalRangePrepare::Handled) {
                return Ok(());
            }
        }
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        self.reject_external_linked_tail_mutation(&inode)?;
        self.ensure_blocks_for_write_range_locked(&mut inode, offset, len)
    }

    /// Commit cached writeback metadata without allocating data blocks.
    ///
    /// The cached-size path is growth-only: truncate uses `setattr()` under
    /// the VFS truncate exclusion. Treating this value as a lower bound keeps
    /// an older frozen fsync from shrinking an EOF already advanced by a
    /// concurrent delayed mapper.
    pub fn commit_inode_metadata(
        &self,
        id: InodeId,
        size: Option<u64>,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        if size.is_some() {
            self.reject_external_linked_tail_mutation(&inode)?;
        }
        if let Some(size) = size {
            if size > inode.inode.size() {
                inode.inode.set_size(size);
            }
        }
        if let Some(atime) = atime {
            inode.inode.set_atime(atime);
        }
        if let Some(mtime) = mtime {
            inode.inode.set_mtime(mtime);
        }
        if let Some(ctime) = ctime {
            inode.inode.set_ctime(ctime);
        }
        self.write_inode_with_csum(&mut inode)?;
        Ok(())
    }

    /// Commit the file size (`i_size`) and optionally `mtime` to disk,
    /// **without** allocating any blocks.
    ///
    /// Call this after successful page-cache write to finalise the new file size.
    pub fn commit_inode_size(&self, id: InodeId, size: u64, mtime: Option<u32>) -> Result<()> {
        self.commit_inode_metadata(id, Some(size), None, mtime, None)
    }

    /// Link a newly created inode into `parent`.
    ///
    /// If linking fails, this function frees the newly allocated inode to avoid leaks.
    fn link_new_inode_or_free(
        &self,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
    ) -> Result<()> {
        match self.link_inode_classified(parent, child, name, false) {
            Ok(()) => Ok(()),
            Err(super::link::LinkFailure::Indeterminate(link_err)) => {
                self.poison(ErrCode::EIO);
                Err(link_err)
            }
            Err(super::link::LinkFailure::Unmodified(link_err)) => {
                if let Err(cleanup_err) = self.free_inode(child) {
                    trace!(
                        "link failed for new inode {} (name {}), cleanup failed: {:?}; original link error: {:?}",
                        child.id,
                        name,
                        cleanup_err,
                        link_err
                    );
                    self.poison(ErrCode::EIO);
                    return Err(cleanup_err);
                }
                Err(link_err)
            }
        }
    }

    /// Create a file. This function will not check the existence of
    /// the file, call `lookup` to check beforehand.
    ///
    /// # Params
    ///
    /// * `parent` - parent directory inode id
    /// * `name` - file name
    /// * `mode` - file type and mode with which to create the new file
    /// * `flags` - open flags
    ///
    /// # Return
    ///
    /// `Ok(inode)` - Inode id of the new file
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - No space left on device
    pub fn create(&self, parent: InodeId, name: &str, mode: InodeMode) -> Result<InodeId> {
        self.create_with_owner(parent, name, mode, InodeOwner { uid: 0, gid: 0 })
    }

    pub fn create_with_owner(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        owner: InodeOwner,
    ) -> Result<InodeId> {
        Ok(self
            .create_with_owner_and_attr(parent, name, mode, owner)?
            .ino)
    }

    /// Create and link a file, returning the attributes from the authoritative
    /// in-memory inode used by the namespace transaction.
    pub fn create_with_owner_and_attr(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        owner: InodeOwner,
    ) -> Result<FileAttr> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent]);
        let mut parent = self.read_inode(parent)?;
        // Can only create a file in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Create child inode and link it to parent directory
        let mut child = self.create_inode_with_owner(mode, owner.uid, owner.gid)?;
        self.link_new_inode_or_free(&mut parent, &mut child, name)?;
        Ok(Self::file_attr(&child))
    }

    /// Create a symbolic link whose target is initialized before its name is
    /// published in the parent directory.
    pub fn symlink_with_owner_and_attr(
        &self,
        parent: InodeId,
        name: &str,
        target: &str,
        owner: InodeOwner,
    ) -> Result<FileAttr> {
        self.ensure_mutable()?;
        if target.is_empty() {
            return_error!(ErrCode::ENOENT, "Symbolic link target is empty");
        }
        if target.len() >= PATH_MAX {
            return_error!(ErrCode::ENAMETOOLONG, "Symbolic link target is too long");
        }

        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent]);
        let mut parent = self.read_inode(parent)?;
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }

        let mode = InodeMode::SOFTLINK | InodeMode::ALL_RWX;
        let mut child = self.create_inode_with_owner(mode, owner.uid, owner.gid)?;
        let initialized = if target.len() + 1 <= child.inode.inline_block().len() {
            child
                .inode
                .set_fast_symlink(target.as_bytes())
                .and_then(|_| self.write_inode_with_csum(&mut child))
        } else {
            let mut image = Box::new([0; BLOCK_SIZE]);
            image[..target.len()].copy_from_slice(target.as_bytes());
            self.extent_query_or_create_initialized(&mut child, 0, 1, Some(image))
                .and_then(|_| self.recompute_inode_block_count(&mut child))
                .and_then(|_| {
                    child.inode.set_size(target.len() as u64);
                    self.write_inode_with_csum(&mut child)
                })
        };
        if let Err(init_error) = initialized {
            if let Err(cleanup_error) = self.free_inode(&mut child) {
                self.poison(ErrCode::EIO);
                return Err(cleanup_error);
            }
            return Err(init_error);
        }

        self.link_new_inode_or_free(&mut parent, &mut child, name)?;
        Ok(Self::file_attr(&child))
    }

    /// Create a device node (character or block device).
    ///
    /// Unlike `create()`, this function:
    /// - Does NOT initialize the extent tree
    /// - Stores the device number in i_block[0..1] (Linux ext4 standard)
    ///
    /// # Params
    ///
    /// * `parent` - parent directory inode id
    /// * `name` - device node name
    /// * `mode` - file type (must include CHARDEV or BLOCKDEV) and permissions
    /// * `major` - major device number
    /// * `minor` - minor device number
    ///
    /// # Return
    ///
    /// `Ok(inode)` - Inode id of the new device node
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - No space left on device
    pub fn mknod(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        major: u32,
        minor: u32,
    ) -> Result<InodeId> {
        self.mknod_with_owner(
            parent,
            name,
            mode,
            major,
            minor,
            InodeOwner { uid: 0, gid: 0 },
        )
    }

    pub fn mknod_with_owner(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        major: u32,
        minor: u32,
        owner: InodeOwner,
    ) -> Result<InodeId> {
        Ok(self
            .mknod_with_owner_and_attr(parent, name, mode, major, minor, owner)?
            .ino)
    }

    /// Create and link a device node, returning the attributes from the
    /// authoritative in-memory inode used by the namespace transaction.
    pub fn mknod_with_owner_and_attr(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        major: u32,
        minor: u32,
        owner: InodeOwner,
    ) -> Result<FileAttr> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent]);
        let mut parent_ref = self.read_inode(parent)?;

        // Can only create in a directory
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }

        // Create device inode (uses create_device_inode which sets device number)
        let mut child = self.create_device_inode(mode, major, minor, owner.uid, owner.gid)?;

        // Link to parent directory
        self.link_new_inode_or_free(&mut parent_ref, &mut child, name)?;

        trace!("mknod {} ({}:{}) -> inode {}", name, major, minor, child.id);
        Ok(Self::file_attr(&child))
    }

    /// Read data from a file. This function will read exactly `buf.len()`
    /// bytes unless the end of the file is reached.
    ///
    /// # Params
    ///
    /// * `file` - the file handler, acquired by `open` or `create`
    /// * `offset` - offset to read from
    /// * `buf` - the buffer to store the data
    ///
    /// # Return
    ///
    /// `Ok(usize)` - the actual number of bytes read
    ///
    /// # Error
    ///
    /// * `EISDIR` - `file` is not a regular file
    pub fn read(&self, file: InodeId, offset: usize, buf: &mut [u8]) -> Result<usize> {
        // Get the inode of the file
        let file = self.read_inode(file)?;
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }

        // Read no bytes
        if buf.is_empty() {
            return Ok(0);
        }
        let file_size = file.inode.size() as usize;
        if offset >= file_size {
            return Ok(0);
        }
        // Calc the actual size to read
        let read_size = min(buf.len(), file_size - offset);
        // Calc the start block of reading
        let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
        // Calc the length that is not aligned to the block size
        let misaligned = offset % BLOCK_SIZE;

        let mut cursor = 0;
        let mut iblock = start_iblock;
        // Read first block
        if misaligned > 0 {
            let read_len = min(BLOCK_SIZE - misaligned, read_size);
            self.read_extent_or_hole(
                &file,
                start_iblock,
                misaligned,
                &mut buf[cursor..cursor + read_len],
            )?;
            cursor += read_len;
            iblock += 1;
        }
        // Continue with full block reads
        while cursor < read_size {
            let read_len = min(BLOCK_SIZE, read_size - cursor);
            self.read_extent_or_hole(&file, iblock, 0, &mut buf[cursor..cursor + read_len])?;
            cursor += read_len;
            iblock += 1;
        }

        Ok(cursor)
    }

    /// Read the target path of a symbolic link (i.e. readlink(2) semantics).
    ///
    /// - Returns the raw byte sequence of the link content (not required to end with '\0')
    /// - For fast symlink (length <= 60), content is stored in inode.i_block (here inode.block[60])
    /// - For non-fast symlink, content is stored in data blocks, reusing extent read path
    pub fn readlink(&self, inode_id: InodeId, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let inode_ref = self.read_inode(inode_id)?;
        if !inode_ref.inode.is_softlink() {
            return_error!(ErrCode::EINVAL, "Inode {} is not a symlink", inode_id);
        }
        if buf.is_empty() {
            return Ok(0);
        }

        let size = inode_ref.inode.size() as usize;
        if offset >= size {
            return Ok(0);
        }

        // fast symlink: content stored inline in inode.i_block
        let inline = inode_ref.inode.inline_block();
        if size <= inline.len() && inode_ref.inode.fs_block_count() == 0 {
            let n = core::cmp::min(buf.len(), size - offset);
            buf[..n].copy_from_slice(&inline[offset..offset + n]);
            return Ok(n);
        }

        // non-fast symlink: stored in data blocks, reuse extent-based read logic
        let read_size = min(buf.len(), size - offset);
        let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
        let misaligned = offset % BLOCK_SIZE;

        let mut cursor = 0;
        let mut iblock = start_iblock;
        if misaligned > 0 {
            let read_len = min(BLOCK_SIZE - misaligned, read_size);
            self.read_extent_or_hole(
                &inode_ref,
                start_iblock,
                misaligned,
                &mut buf[cursor..cursor + read_len],
            )?;
            cursor += read_len;
            iblock += 1;
        }
        while cursor < read_size {
            let read_len = min(BLOCK_SIZE, read_size - cursor);
            self.read_extent_or_hole(&inode_ref, iblock, 0, &mut buf[cursor..cursor + read_len])?;
            cursor += read_len;
            iblock += 1;
        }

        Ok(cursor)
    }

    /// Write data to a file. This function will write exactly `data.len()` bytes.
    ///
    /// # Params
    ///
    /// * `file` - the file handler, acquired by `open` or `create`
    /// * `offset` - offset to write to
    /// * `data` - the data to write
    ///
    /// # Return
    ///
    /// `Ok(usize)` - the actual number of bytes written
    ///
    /// # Error
    ///
    /// * `EISDIR` - `file` is not a regular file
    /// * `ENOSPC` - no space left on device
    pub fn write(&self, file: InodeId, offset: usize, data: &[u8]) -> Result<usize> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let write_size = data.len();
        if write_size == 0 {
            return Ok(0);
        }
        // Get the inode of the file
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(file)].lock();
        let mut file = self.read_inode(file)?;
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }
        self.reject_external_linked_tail_mutation(&file)?;

        self.ensure_blocks_for_write_range_locked(&mut file, offset, write_size)?;

        // Write data
        let mut cursor = 0;
        let mut iblock = (offset / BLOCK_SIZE) as LBlockId;
        while cursor < write_size {
            let block_offset = (offset + cursor) % BLOCK_SIZE;
            let write_len = min(BLOCK_SIZE - block_offset, write_size - cursor);
            let fblock = self.extent_query(&file, iblock)?;
            let mut block = self.read_block(fblock)?;
            block.write_offset(block_offset, &data[cursor..cursor + write_len]);
            self.write_block(&block)?;
            cursor += write_len;
            iblock += 1;
        }
        let new_end = offset.checked_add(cursor).ok_or(format_error!(
            ErrCode::EFBIG,
            "write end overflow: offset={} len={}",
            offset,
            cursor
        ))?;
        if new_end > file.inode.size() as usize {
            file.inode.set_size(new_end as u64);
        }
        self.write_inode_with_csum(&mut file)?;

        Ok(cursor)
    }

    /// Write data to pre-allocated blocks without modifying inode metadata.
    ///
    /// This is used by page cache writeback: blocks are already allocated by
    /// `prepare_buffered_write` in the foreground `write_at` path; the writeback
    /// thread only needs to push dirty page data to the corresponding
    /// physical blocks.
    ///
    /// Unlike `write()`, this function:
    /// - Does **not** allocate blocks (`inode_append_block`)
    /// - Does **not** update inode size or write inode back to disk
    /// - Returns `ENOENT` if a required logical block has no extent mapping
    ///
    /// This eliminates the race between foreground `setattr` block-allocation
    /// and background writeback, which can corrupt the extent tree when both
    /// operate on cloned `InodeRef` snapshots from the inode cache.
    fn write_data_only_checked(&self, file: InodeId, offset: usize, data: &[u8]) -> Result<usize> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let write_size = data.len();
        let mut chunks = Vec::new();
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(file)].lock();
        let file = self.read_inode(file)?;
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }
        self.reject_external_linked_tail_mutation(&file)?;

        let mut cursor = 0;
        let mut iblock = (offset / BLOCK_SIZE) as LBlockId;
        while cursor < write_size {
            let block_offset = (offset + cursor) % BLOCK_SIZE;
            let write_len = min(BLOCK_SIZE - block_offset, write_size - cursor);
            match self.extent_query(&file, iblock) {
                Ok(fblock) => {
                    chunks.push((fblock, block_offset, cursor, write_len));
                }
                Err(e) => {
                    debug!(
                            "write_data_only: extent_query FAILED ino={} iblock={} offset={} len={} fs_blkcnt={} size={} err={:?}",
                            file.id, iblock, offset, write_size,
                            file.inode.fs_block_count(), file.inode.size(), e
                        );
                    return Err(e);
                }
            }
            cursor += write_len;
            iblock += 1;
        }

        for (fblock, block_offset, cursor, write_len) in chunks {
            if block_offset == 0 && write_len == BLOCK_SIZE {
                // Page-cache writeback supplies complete, block-aligned pages.
                // Reading the old block before overwriting every byte doubles
                // virtio I/O and serves no correctness purpose.
                self.block_device
                    .write_blocks(fblock, &data[cursor..cursor + write_len])?;
            } else {
                let mut block = self.read_block(fblock)?;
                block.write_offset(block_offset, &data[cursor..cursor + write_len]);
                self.write_block(&block)?;
            }
        }

        Ok(write_size)
    }

    /// Write data to pre-allocated blocks without modifying inode metadata.
    pub fn write_data_only(&self, file: InodeId, offset: usize, data: &[u8]) -> Result<usize> {
        self.write_data_only_checked(file, offset, data)
    }

    /// Verify the mapped receipt while the direct metadata gate and inode
    /// shard exclude unlink/reclaim, then write its exact block payload. This
    /// prevents a stale receipt from reaching an inode number that was reused
    /// by a later allocation before the EOF/orphan transaction can reject it.
    #[cfg(any(test, feature = "test-api"))]
    fn write_delalloc_mapped_data(
        &self,
        receipt: &DelallocMappedWriteback,
        data: &[u8],
    ) -> core::result::Result<(), DelallocMappedDataFailure> {
        let _metadata_guard = match self.lock_direct_metadata_mutation() {
            Ok(guard) => guard,
            Err(error) if error.code() == ErrCode::EAGAIN => {
                return Err(DelallocMappedDataFailure::Retryable(error));
            }
            Err(error) => return Err(DelallocMappedDataFailure::Fatal(error)),
        };
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(receipt.inode_id)].lock();
        let inode = self
            .read_inode(receipt.inode_id)
            .map_err(DelallocMappedDataFailure::Fatal)?;
        if !inode.inode.is_file()
            || inode.inode.generation() != receipt.inode_generation
            || inode.inode.size() != receipt.offset as u64
        {
            return Err(DelallocMappedDataFailure::Fatal(Ext4Error::new(
                ErrCode::EIO,
            )));
        }
        match self.legacy_orphan_membership(&inode) {
            Ok(LegacyOrphanMembership::LinkedTail) => {}
            Ok(_) => {
                return Err(DelallocMappedDataFailure::Fatal(Ext4Error::new(
                    ErrCode::EIO,
                )));
            }
            Err(error) => return Err(DelallocMappedDataFailure::Fatal(error)),
        }
        let lblock = (receipt.offset / BLOCK_SIZE) as LBlockId;
        let pblock = self
            .extent_query(&inode, lblock)
            .map_err(DelallocMappedDataFailure::Fatal)?;
        self.block_device
            .write_blocks(pblock, data)
            .map_err(DelallocMappedDataFailure::Retryable)
    }

    /// Create a hard link. This function will not check name conflict,
    /// call `lookup` to check beforehand.
    ///
    /// # Params
    ///
    /// * `child` - the inode of the file to link
    /// * `parent` - the inode of the directory to link to
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - no space left on device
    pub fn link(&self, child: InodeId, parent: InodeId, name: &str) -> Result<()> {
        self.ensure_mutable()?;
        // Relinking a zero-link inode must compose namespace publication with
        // orphan removal in one journal transaction.  Use the exclusive
        // metadata domain for both zero and nonzero link-count cases.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent, child]);
        let mut parent = self.read_inode(parent)?;
        // Can only link to a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        let mut child = self.read_inode(child)?;
        // Cannot link a directory
        if child.inode.is_dir() {
            return_error!(ErrCode::EISDIR, "Cannot link a directory");
        }
        self.link_inode(&mut parent, &mut child, name, true)?;
        Ok(())
    }

    /// Unlink a file.
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the directory to unlink from
    /// * `name` - the name of the file to unlink
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOENT` - `name` does not exist in `parent`
    /// * `EISDIR` - `parent/name` is a directory
    pub fn unlink(&self, parent: InodeId, name: &str) -> Result<Option<InodeReclaimHandle>> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let mut parent_ref = self.read_inode(parent)?;
        // Can only unlink from a directory
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }
        // Cannot unlink directory
        let child_id = self.dir_find_entry(&parent_ref, name)?;
        let _mutation_guards = self.lock_inode_mutations(&[parent, child_id]);
        parent_ref = self.read_inode(parent)?;
        if self.dir_find_entry(&parent_ref, name)? != child_id {
            return_error!(ErrCode::ENOENT, "Namespace changed during unlink");
        }
        let mut child = self.read_inode(child_id)?;
        if child.inode.is_dir() {
            return_error!(ErrCode::EISDIR, "Cannot unlink a directory");
        }
        self.unlink_inode(&mut parent_ref, &mut child, name)
    }

    /// Helper: Read and validate parent directories for rename operations.
    ///
    /// Returns (parent_ref, Option<new_parent_ref>). If parent == new_parent,
    /// the second element is None to avoid double-locking the same inode.
    fn read_rename_dirs(
        &self,
        parent: InodeId,
        new_parent: InodeId,
    ) -> Result<(InodeRef, Option<InodeRef>)> {
        let parent_ref = self.read_inode(parent)?;
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }

        let new_parent_ref = if parent == new_parent {
            None
        } else {
            let np = self.read_inode(new_parent)?;
            if !np.inode.is_dir() {
                return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", np.id);
            }
            Some(np)
        };

        Ok((parent_ref, new_parent_ref))
    }

    /// Helper: Check if `target_dir` is a descendant of `dir_inode`.
    ///
    /// Used to prevent directory cycles in rename operations.
    /// Returns EINVAL if moving a directory into its own subdirectory.
    fn check_ancestor_cycle(&self, dir_inode: InodeId, target_dir: InodeId) -> Result<()> {
        let mut cur = target_dir;
        loop {
            if cur == dir_inode {
                return_error!(
                    ErrCode::EINVAL,
                    "Cannot move directory into its own subdirectory"
                );
            }
            if cur == EXT4_ROOT_INO {
                break;
            }
            let cur_inode = self.read_inode(cur)?;
            match self.dir_find_entry(&cur_inode, "..") {
                Ok(parent_id) if parent_id != cur => cur = parent_id,
                _ => break,
            }
        }
        Ok(())
    }

    /// Rename a directory entry, with POSIX-compliant atomic replace semantics.
    ///
    /// # POSIX Semantics
    /// - If `new_name` doesn't exist: simple rename
    /// - If `new_name` exists and is the same inode as source: no-op, return Ok
    /// - If `new_name` exists and is different inode: **atomically replace** it
    /// - Directory can only replace empty directory
    /// - Type compatibility: file<->file, dir<->dir (no cross-type replace)
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the source directory
    /// * `name` - the name of the file to move
    /// * `new_parent` - the inode of the directory to move to
    /// * `new_name` - the new name of the file
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` or `new_parent` is not a directory, or dir replacing non-dir
    /// * `ENOENT` - `name` does not exist in `parent`
    /// * `EISDIR` - non-dir replacing dir
    /// * `ENOTEMPTY` - target directory is not empty
    /// * `EINVAL` - would create a directory cycle (moving dir into its own subdirectory)
    /// * `ENOSPC` - no space left on device
    pub fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<Option<InodeReclaimHandle>> {
        self.ensure_mutable()?;
        // Rename can remove the final name of an overwritten target. Keep the
        // complete namespace transition in the exclusive domain so a follow-up
        // transactional orphan/reclaim implementation cannot inherit a stale
        // direct-writer snapshot window.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let mut reclaim = None;
        // 1. 验证父目录
        let (mut parent_ref, mut new_parent_ref) = self.read_rename_dirs(parent, new_parent)?;

        // 2. 查找源 inode
        let child_id = self.dir_find_entry(&parent_ref, name)?;
        let mut child = self.read_inode(child_id)?;
        let child_is_dir = child.inode.is_dir();

        // 3. 循环检测：防止把目录移到自己的子目录下
        if child_is_dir && parent != new_parent {
            self.check_ancestor_cycle(child_id, new_parent)?;
        }

        // 4. 检查目标是否存在
        let target_dir_ref = new_parent_ref.as_ref().unwrap_or(&parent_ref);
        let existing = self.dir_find_entry(target_dir_ref, new_name).ok();
        let mut mutation_ids = vec![parent, new_parent, child_id];
        if let Some(existing_id) = existing {
            mutation_ids.push(existing_id);
        }
        let _mutation_guards = self.lock_inode_mutations(&mutation_ids);
        parent_ref = self.read_inode(parent)?;
        new_parent_ref = if parent == new_parent {
            None
        } else {
            Some(self.read_inode(new_parent)?)
        };
        child = self.read_inode(child_id)?;
        let child_file_type = child.inode.file_type();

        match existing {
            Some(existing_id) if existing_id == child_id => {
                // 情况 A：源和目标是同一个 inode（硬链接或同名）
                // POSIX 语义：无操作，返回成功
                return Ok(None);
            }
            Some(existing_id) => {
                // 情况 B：目标存在且是不同 inode → 原子替换
                let mut existing_inode = self.read_inode(existing_id)?;
                let existing_is_dir = existing_inode.inode.is_dir();

                // 4b-1. 类型兼容性检查
                match (child_is_dir, existing_is_dir) {
                    (true, false) => {
                        return_error!(
                            ErrCode::ENOTDIR,
                            "Cannot replace non-directory with directory"
                        );
                    }
                    (false, true) => {
                        return_error!(
                            ErrCode::EISDIR,
                            "Cannot replace directory with non-directory"
                        );
                    }
                    (true, true) => {
                        // 目录替换目录：目标必须为空
                        if !self.dir_is_empty(&existing_inode)? {
                            return_error!(ErrCode::ENOTEMPTY, "Target directory is not empty");
                        }
                    }
                    (false, false) => {
                        // 文件替换文件：OK
                    }
                }

                let existing_link_cnt = existing_inode.inode.link_count();
                // An empty replaced directory loses its only parent entry and
                // Linux clear_nlink()s it even when an old/corrupt on-disk
                // count is unexpectedly greater than two.  Regular files can
                // still have independent hard links.
                let final_target =
                    super::link::namespace_removal_is_final(existing_is_dir, existing_link_cnt);
                let orphan_action = if final_target && self.uses_journal() {
                    Some(final_unlink_orphan_action(
                        self.legacy_orphan_membership(&existing_inode)?,
                    )?)
                } else {
                    None
                };

                // Upper bound of distinct home blocks in the replace set:
                // destination dirent + source dirent + optional child "..";
                // overwritten inode + each logically changed parent inode;
                // and the superblock only for a final target.  The transaction
                // map deduplicates entries which share a directory or inode-
                // table block, so same-parent and same-block cases consume
                // fewer credits without weakening the reservation bound.
                let mut credits = 3; // two dirent blocks + overwritten inode
                if child_is_dir && parent != new_parent {
                    credits += 3; // child ".." + old parent + new parent
                }
                if existing_is_dir && !(child_is_dir && parent != new_parent) {
                    credits += 1; // target parent (new parent already counted above)
                }
                if final_target && self.uses_journal() {
                    credits += 1; // superblock orphan head
                }
                let mut transaction = self.transaction_start(credits)?;

                // Match Linux ext4_rename(): ext4_setent(new), delete(old),
                // ext4_rename_dir_finish(), parent counts, target nlink, and
                // ext4_orphan_add() all belong to this single handle.
                {
                    let target_dir = new_parent_ref.as_mut().unwrap_or(&mut parent_ref);
                    self.transaction_dir_replace_entry(
                        &mut transaction,
                        target_dir,
                        new_name,
                        child_id,
                        child_file_type,
                    )?;

                    if existing_is_dir {
                        target_dir
                            .inode
                            .set_link_count(target_dir.inode.link_count() - 1);
                        self.transaction_stage_inode_with_csum(&mut transaction, target_dir)?;
                    }
                }

                self.transaction_dir_remove_entry(&mut transaction, &parent_ref, name)?;

                if child_is_dir && parent != new_parent {
                    self.transaction_dir_replace_entry(
                        &mut transaction,
                        &child,
                        "..",
                        new_parent,
                        FileType::Directory,
                    )?;

                    parent_ref
                        .inode
                        .set_link_count(parent_ref.inode.link_count() - 1);
                    self.transaction_stage_inode_with_csum(&mut transaction, &mut parent_ref)?;

                    let new_parent_dir = new_parent_ref.as_mut().ok_or(format_error!(
                        ErrCode::EINVAL,
                        "rename: missing new parent reference for directory move"
                    ))?;
                    new_parent_dir
                        .inode
                        .set_link_count(new_parent_dir.inode.link_count() + 1);
                    self.transaction_stage_inode_with_csum(&mut transaction, new_parent_dir)?;
                }

                if final_target {
                    existing_inode.inode.set_link_count(0);
                    if self.uses_journal() {
                        let mut sb = self.read_super_block_cached();
                        let orphan_action =
                            orphan_action.ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                        match orphan_action {
                            FinalUnlinkOrphanAction::AddZeroLink => {
                                self.transaction_orphan_add_zero_link(
                                    &mut transaction,
                                    &mut existing_inode,
                                    &mut sb,
                                )?;
                            }
                            FinalUnlinkOrphanAction::PreserveLinkedTail => {
                                self.transaction_stage_inode_with_csum(
                                    &mut transaction,
                                    &mut existing_inode,
                                )?;
                            }
                        }
                    } else {
                        existing_inode.inode.set_next_orphan(0);
                        self.transaction_stage_inode_with_csum(
                            &mut transaction,
                            &mut existing_inode,
                        )?;
                    }
                } else {
                    existing_inode.inode.set_link_count(existing_link_cnt - 1);
                    self.transaction_stage_inode_with_csum(&mut transaction, &mut existing_inode)?;
                }

                if let Err(error) = transaction.commit(self.block_device.as_ref(), self) {
                    // Once commit processing starts, failures can leave an
                    // uncertain committed/checkpointed state.  Fail-stop every
                    // subsequent metadata writer on this mount.
                    self.poison(ErrCode::EIO);
                    return Err(error.error);
                }
                if final_target {
                    reclaim = Some(InodeReclaimHandle::new(
                        existing_inode.id,
                        existing_inode.inode.generation(),
                    ));
                }
                // 文件的 link count 不变（只是换了名字/位置）
            }
            None => {
                // 情况 C：目标不存在 → 简单重命名
                // Without a journal, any failure after the first namespace
                // write fail-stops this mount so a partial rename cannot be
                // followed by further allocation or metadata mutation.

                // C-1. 在目标父目录添加新条目（先 add）
                let target_dir = new_parent_ref.as_mut().unwrap_or(&mut parent_ref);
                match self.dir_add_entry_classified(target_dir, &child, new_name) {
                    Ok(()) => {}
                    Err(super::dir::DirAddFailure::Unmodified(error)) => return Err(error),
                    Err(super::dir::DirAddFailure::Indeterminate(error)) => {
                        self.poison(ErrCode::EIO);
                        return Err(error);
                    }
                }

                // C-2. 从源父目录删除旧条目（后 delete）
                self.poison_on_error(self.dir_remove_entry(&parent_ref, name))?;

                // C-3. 目录跨目录移动时，原子更新 ".." 并调整 link count
                if child_is_dir && parent != new_parent {
                    // ".." 原地替换：旧父 → 新父，单次 I/O，无中间态
                    self.poison_on_error(self.dir_replace_entry(
                        &child,
                        "..",
                        new_parent,
                        FileType::Directory,
                    ))?;

                    // 源父目录失去 ".." 引用
                    parent_ref
                        .inode
                        .set_link_count(parent_ref.inode.link_count() - 1);
                    self.poison_on_error(self.write_inode_with_csum(&mut parent_ref))?;

                    // 目标父目录获得 ".." 引用
                    let new_parent_dir = new_parent_ref.as_mut().ok_or(format_error!(
                        ErrCode::EINVAL,
                        "rename: missing new parent reference for directory move"
                    ))?;
                    new_parent_dir
                        .inode
                        .set_link_count(new_parent_dir.inode.link_count() + 1);
                    self.poison_on_error(self.write_inode_with_csum(new_parent_dir))?;
                }
                // 文件：无 ".."，nlink 不变（只换了名字/位置）
                // 目录同目录：".." 已指向正确的父，link count 不变
            }
        }

        Ok(reclaim)
    }

    /// Atomically exchange two directory entries (RENAME_EXCHANGE semantics).
    ///
    /// Both entries must exist. The operation swaps their inode references
    /// in place using `dir_replace_entry`, so directory entries never "disappear".
    ///
    /// # Params
    ///
    /// * `parent` - inode of the directory containing `name`
    /// * `name` - name of the first entry
    /// * `new_parent` - inode of the directory containing `new_name`
    /// * `new_name` - name of the second entry
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` or `new_parent` is not a directory
    /// * `ENOENT` - `name` or `new_name` does not exist
    /// * `EINVAL` - would create a directory cycle
    pub fn rename_exchange(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        // 1. 验证父目录
        let (mut parent_ref, mut new_parent_ref) = self.read_rename_dirs(parent, new_parent)?;

        // 2. 查找两个 inode
        let old_id = self.dir_find_entry(&parent_ref, name)?;
        let target_dir_ref = new_parent_ref.as_ref().unwrap_or(&parent_ref);
        let new_id = self.dir_find_entry(target_dir_ref, new_name)?;
        let _mutation_guards = self.lock_inode_mutations(&[parent, new_parent, old_id, new_id]);
        parent_ref = self.read_inode(parent)?;
        new_parent_ref = if parent == new_parent {
            None
        } else {
            Some(self.read_inode(new_parent)?)
        };
        let old_inode = self.read_inode(old_id)?;
        let old_is_dir = old_inode.inode.is_dir();
        let old_type = old_inode.inode.file_type();
        let new_inode = self.read_inode(new_id)?;
        let new_is_dir = new_inode.inode.is_dir();
        let new_type = new_inode.inode.file_type();

        // 3. 同一 inode → 无操作
        if old_id == new_id {
            return Ok(());
        }

        // 4. 循环检测（仅跨目录时需要，exchange 需要检查双向）
        if parent != new_parent {
            if old_is_dir {
                self.check_ancestor_cycle(old_id, new_parent)?;
            }
            if new_is_dir {
                self.check_ancestor_cycle(new_id, parent)?;
            }
        }

        // 5. 原子交换：原地替换目录项的 inode 引用
        if parent == new_parent {
            self.poison_on_error(self.dir_replace_entry(&parent_ref, name, new_id, new_type))?;
            self.poison_on_error(self.dir_replace_entry(&parent_ref, new_name, old_id, old_type))?;
        } else {
            self.poison_on_error(self.dir_replace_entry(&parent_ref, name, new_id, new_type))?;
            let new_parent_dir = new_parent_ref.as_ref().ok_or(format_error!(
                ErrCode::EINVAL,
                "rename_exchange: missing new parent reference for cross-dir exchange"
            ))?;
            self.poison_on_error(self.dir_replace_entry(
                new_parent_dir,
                new_name,
                old_id,
                old_type,
            ))?;
        }

        // 6. 跨目录时更新目录的 ".." 指向和父目录 link_count
        if parent != new_parent {
            if old_is_dir {
                self.poison_on_error(self.dir_replace_entry(
                    &old_inode,
                    "..",
                    new_parent,
                    FileType::Directory,
                ))?;
                parent_ref
                    .inode
                    .set_link_count(parent_ref.inode.link_count() - 1);
                self.poison_on_error(self.write_inode_with_csum(&mut parent_ref))?;
                let np = new_parent_ref.as_mut().ok_or(format_error!(
                    ErrCode::EINVAL,
                    "rename_exchange: missing new parent reference for old_dir update"
                ))?;
                np.inode.set_link_count(np.inode.link_count() + 1);
                self.poison_on_error(self.write_inode_with_csum(np))?;
            }
            if new_is_dir {
                self.poison_on_error(self.dir_replace_entry(
                    &new_inode,
                    "..",
                    parent,
                    FileType::Directory,
                ))?;
                let np = new_parent_ref.as_mut().ok_or(format_error!(
                    ErrCode::EINVAL,
                    "rename_exchange: missing new parent reference for new_dir update"
                ))?;
                np.inode.set_link_count(np.inode.link_count() - 1);
                self.poison_on_error(self.write_inode_with_csum(np))?;
                parent_ref
                    .inode
                    .set_link_count(parent_ref.inode.link_count() + 1);
                self.poison_on_error(self.write_inode_with_csum(&mut parent_ref))?;
            }
        }

        Ok(())
    }

    /// Create a directory. This function will not check name conflict,
    /// call `lookup` to check beforehand.
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the directory to create in
    /// * `name` - the name of the directory to create
    /// * `mode` - the mode of the directory to create, type field will be ignored
    ///
    /// # Return
    ///
    /// `Ok(child)` - the inode id of the created directory
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - no space left on device
    pub fn mkdir(&self, parent: InodeId, name: &str, mode: InodeMode) -> Result<InodeId> {
        self.mkdir_with_owner(parent, name, mode, InodeOwner { uid: 0, gid: 0 })
    }

    pub fn mkdir_with_owner(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        owner: InodeOwner,
    ) -> Result<InodeId> {
        Ok(self
            .mkdir_with_owner_and_attr(parent, name, mode, owner)?
            .ino)
    }

    /// Create and link a directory, returning the attributes from the
    /// authoritative in-memory inode used by the namespace transaction.
    pub fn mkdir_with_owner_and_attr(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        owner: InodeOwner,
    ) -> Result<FileAttr> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent]);
        let mut parent = self.read_inode(parent)?;
        // Can only create a directory in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Create file/directory
        let mode = mode & InodeMode::PERM_MASK | InodeMode::DIRECTORY;
        let mut child = self.create_inode_with_owner(mode, owner.uid, owner.gid)?;
        // Add "." entry
        let child_self = child.clone();
        if let Err(error) = self.dir_add_entry(&mut child, &child_self, ".") {
            if self.free_inode(&mut child).is_err() {
                self.poison(ErrCode::EIO);
            }
            return Err(error);
        }
        child.inode.set_link_count(1);
        // Link the new inode
        self.link_new_inode_or_free(&mut parent, &mut child, name)?;
        Ok(Self::file_attr(&child))
    }

    /// Look up a directory entry by name.
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the directory to look in
    /// * `name` - the name of the entry to look for
    ///
    /// # Return
    ///
    /// `Ok(child)`- the inode id to which the directory entry points.
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOENT` - `name` does not exist in `parent`
    pub fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId> {
        let parent = self.read_inode(parent)?;
        // Can only lookup in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        self.dir_find_entry(&parent, name)
    }

    /// List all directory entries in a directory.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the directory to list
    ///
    /// # Return
    ///
    /// `Ok(entries)` - a vector of directory entries in the directory.
    ///
    /// # Error
    ///
    /// `ENOTDIR` - `inode` is not a directory
    pub fn listdir(&self, inode: InodeId) -> Result<Vec<DirEntry>> {
        let inode_ref = self.read_inode(inode)?;
        // Can only list a directory
        if inode_ref.inode.file_type() != FileType::Directory {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", inode);
        }
        self.dir_list_entries(&inode_ref)
    }

    /// Remove an empty directory.
    ///
    /// # Params
    ///
    /// * `parent` - the parent directory where the directory is located
    /// * `name` - the name of the directory to remove
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` or `child` is not a directory
    /// * `ENOENT` - `name` does not exist in `parent`
    /// * `ENOTEMPTY` - `child` is not empty
    pub fn rmdir(&self, parent: InodeId, name: &str) -> Result<Option<InodeReclaimHandle>> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let mut parent_ref = self.read_inode(parent)?;
        // Can only remove a directory in a directory
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }
        let child_id = self.dir_find_entry(&parent_ref, name)?;
        let _mutation_guards = self.lock_inode_mutations(&[parent, child_id]);
        parent_ref = self.read_inode(parent)?;
        if self.dir_find_entry(&parent_ref, name)? != child_id {
            return_error!(ErrCode::ENOENT, "Namespace changed during rmdir");
        }
        let mut child = self.read_inode(child_id)?;
        // Child must be a directory
        if !child.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", child.id);
        }
        // Child must be empty
        if self.dir_list_entries(&child)?.len() > 2 {
            return_error!(ErrCode::ENOTEMPTY, "Directory {} is not empty", child.id);
        }
        // Remove directory entry
        self.unlink_inode(&mut parent_ref, &mut child, name)
    }

    /// Get extended attribute of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    /// * `name` - the name of the attribute
    ///
    /// # Return
    ///
    /// `Ok(value)` - the value of the attribute
    ///
    /// # Error
    ///
    /// `ENODATA` - the attribute does not exist
    pub fn getxattr(&self, inode: InodeId, name: &str) -> Result<Vec<u8>> {
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        match xattr_block.get(name) {
            Some(value) => Ok(value.to_owned()),
            None => Err(format_error!(
                ErrCode::ENODATA,
                "Xattr {} does not exist",
                name
            )),
        }
    }

    /// Set extended attribute of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    /// * `name` - the name of the attribute
    /// * `value` - the value of the attribute
    ///
    /// # Error
    ///
    /// `ENOSPC` - xattr block does not have enough space
    pub fn setxattr(&self, inode: InodeId, name: &str, value: &[u8]) -> Result<()> {
        self.ensure_mutable()?;
        self.setxattr_with_flags(inode, name, value, false, false)
    }

    /// Set extended attribute of a file with Linux create/replace semantics.
    ///
    /// Existing xattr blocks are modified on a cloned candidate block first and
    /// written back only after the whole operation succeeds. This preserves the
    /// old value when replacing with a value that does not fit.
    pub fn setxattr_with_flags(
        &self,
        inode: InodeId,
        name: &str,
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode)].lock();
        let mut inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            if replace {
                return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
            }
            // lazy allocate xattr block
            let pblock = self.alloc_block(&mut inode_ref)?;
            let old_xattr_block = xattr_block_id;
            let result = (|| {
                let mut xattr_block = XattrBlock::new(self.read_block(pblock)?);
                xattr_block.init();
                if !xattr_block.insert(name, value) {
                    return_error!(
                        ErrCode::ENOSPC,
                        "Xattr block of Inode {} does not have enough space",
                        inode
                    );
                }
                self.update_xattr_block_checksum(pblock, &mut xattr_block)?;
                self.write_block(&xattr_block.block())?;
                inode_ref.inode.set_xattr_block(pblock);
                self.write_inode_with_csum(&mut inode_ref)?;
                Ok(())
            })();
            if let Err(err) = result {
                inode_ref.inode.set_xattr_block(old_xattr_block);
                return match self.dealloc_block(&mut inode_ref, pblock) {
                    Ok(()) => Err(err),
                    Err(rollback_err) => Err(rollback_err),
                };
            }
            return Ok(());
        }

        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        let exists = xattr_block.get(name).is_some();
        if exists && create {
            return_error!(ErrCode::EEXIST, "Xattr {} already exists", name);
        }
        if !exists && replace {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }

        let mut new_xattr_block = xattr_block;
        if exists {
            let _ = new_xattr_block.remove(name);
        }
        if new_xattr_block.insert(name, value) {
            self.update_xattr_block_checksum(xattr_block_id, &mut new_xattr_block)?;
            self.write_block(&new_xattr_block.block())?;
            Ok(())
        } else {
            return_error!(
                ErrCode::ENOSPC,
                "Xattr block of Inode {} does not have enough space",
                inode
            );
        }
    }

    /// Remove extended attribute of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    /// * `name` - the name of the attribute
    ///
    /// # Error
    ///
    /// `ENODATA` - the attribute does not exist
    pub fn removexattr(&self, inode: InodeId, name: &str) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode)].lock();
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let mut xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        if xattr_block.remove(name) {
            self.update_xattr_block_checksum(xattr_block_id, &mut xattr_block)?;
            self.write_block(&xattr_block.block())?;
            Ok(())
        } else {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
    }

    /// List extended attributes of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    ///
    /// # Returns
    ///
    /// A list of extended attributes of the file.
    pub fn listxattr(&self, inode: InodeId) -> Result<Vec<String>> {
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return Ok(Vec::new());
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        Ok(xattr_block.list())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext4::{MetadataMutationMode, MetadataMutationWaker};
    use crate::FileType;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountingMetadataWaker {
        wakes: AtomicUsize,
    }

    impl MetadataMutationWaker for CountingMetadataWaker {
        fn wake_all(&self) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn production_credit_bound_violation_is_a_terminal_contract_error() {
        assert_eq!(
            validate_delalloc_journal_credit_bound(11, Some(10))
                .unwrap_err()
                .code(),
            ErrCode::E2BIG
        );
        assert!(is_delalloc_contract_error(ErrCode::E2BIG));
        assert!(!is_delalloc_contract_error(ErrCode::EIO));
        assert_eq!(
            validate_delalloc_journal_credit_bound(10, Some(10)).unwrap(),
            10
        );
        assert_eq!(
            validate_delalloc_journal_credit_bound(11, None).unwrap(),
            11
        );
    }

    #[test]
    fn file_attr_is_derived_from_the_authoritative_in_memory_inode() {
        let mut inode = Box::new(Inode::default());
        inode.set_mode(InodeMode::CHARDEV | InodeMode::from_bits_retain(0o640));
        inode.set_uid(0x12345);
        inode.set_gid(0x23456);
        inode.set_size(0x1_0000_0020);
        inode.set_block_count(17);
        inode.set_atime(11);
        inode.set_mtime(12);
        inode.set_ctime(13);
        inode.set_crtime(14);
        inode.set_link_count(2);
        inode.set_device(259, 0x1_0002);
        let inode = InodeRef::new(42, inode);

        let attr = Ext4::file_attr(&inode);

        assert_eq!(attr.ino, 42);
        assert_eq!(attr.ftype, FileType::CharacterDev);
        assert_eq!(attr.perm.bits(), 0o640);
        assert_eq!(attr.uid, 0x12345);
        assert_eq!(attr.gid, 0x23456);
        assert_eq!(attr.size, 0x1_0000_0020);
        assert_eq!(attr.blocks, 17);
        assert_eq!(
            (attr.atime, attr.mtime, attr.ctime, attr.crtime),
            (11, 12, 13, 14)
        );
        assert_eq!(attr.links, 2);
        assert_eq!(attr.rdev, (259, 0x1_0002));
    }

    struct StubBlockDevice {
        sb_block: Block,
    }

    impl StubBlockDevice {
        fn with_block_count(block_count: u32) -> Self {
            let mut data = [0u8; BLOCK_SIZE];
            let off = BASE_OFFSET + core::mem::size_of::<u32>();
            data[off..off + 4].copy_from_slice(&block_count.to_le_bytes());
            Self {
                sb_block: Block::new(0, Box::new(data)),
            }
        }
    }

    impl BlockDevice for StubBlockDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            if block_id == 0 {
                Ok(self.sb_block.clone())
            } else {
                Ok(Block::new(block_id, Box::new([0u8; BLOCK_SIZE])))
            }
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    fn make_test_fs(block_count: u32) -> Ext4 {
        let block_device = Arc::new(StubBlockDevice::with_block_count(block_count));
        make_test_fs_with_device(block_count, block_device)
    }

    fn make_test_fs_with_device(block_count: u32, block_device: Arc<dyn BlockDevice>) -> Ext4 {
        let block = block_device.read_block(0).unwrap();
        let sb = block.read_offset_as::<SuperBlock>(BASE_OFFSET);
        Ext4 {
            block_device,
            cached_super_block: spin::Mutex::new(sb),
            cached_block_groups: Vec::new(),
            system_metadata_ranges: Vec::new(),
            inode_cache: spin::Mutex::new(crate::ext4::InodeCache::new(16)),
            alloc_lock: spin::Mutex::new(crate::ext4::AllocationState::new().unwrap()),
            namespace_lock: spin::Mutex::new(()),
            metadata_mutation_barrier: crate::ext4::MetadataMutationGate::new(),
            poisoned: spin::Mutex::new(None),
            delalloc_mapper_authority_issued: core::sync::atomic::AtomicBool::new(false),
            metadata_mode: crate::ext4::MetadataMutationMode::Direct(
                crate::ext4::journal_transaction::DirectTransactionCore::new(block_count as u64)
                    .unwrap(),
            ),
            write_barrier: true,
            direct_restore_clean: false,
            inode_mutation_locks: (0..crate::ext4::INODE_MUTATION_LOCK_SHARDS)
                .map(|_| spin::Mutex::new(()))
                .collect(),
            prepare_stats: crate::ext4::PrepareStats::new(),
        }
    }

    struct RangeInitDevice {
        sb_block: Block,
        writes: AtomicUsize,
        flushes: AtomicUsize,
        fail_write_at: AtomicUsize,
        fail_flush: AtomicBool,
    }

    impl RangeInitDevice {
        fn new(block_count: u32) -> Self {
            let stub = StubBlockDevice::with_block_count(block_count);
            Self {
                sb_block: stub.sb_block,
                writes: AtomicUsize::new(0),
                flushes: AtomicUsize::new(0),
                fail_write_at: AtomicUsize::new(usize::MAX),
                fail_flush: AtomicBool::new(false),
            }
        }
    }

    impl BlockDevice for RangeInitDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            if block_id == 0 {
                Ok(self.sb_block.clone())
            } else {
                Ok(Block::new(block_id, Box::new([0; BLOCK_SIZE])))
            }
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            Ok(())
        }

        fn write_blocks(&self, _start: PBlockId, _data: &[u8]) -> Result<()> {
            let write = self.writes.fetch_add(1, Ordering::SeqCst);
            if write == self.fail_write_at.load(Ordering::SeqCst) {
                Err(Ext4Error::new(ErrCode::ENOMEM))
            } else {
                Ok(())
            }
        }

        fn flush(&self) -> Result<()> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            if self.fail_flush.load(Ordering::SeqCst) {
                Err(Ext4Error::new(ErrCode::EIO))
            } else {
                Ok(())
            }
        }

        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    #[test]
    fn allocated_range_zero_failure_stops_before_flush() {
        let device = Arc::new(RangeInitDevice::new(128));
        device.fail_write_at.store(1, Ordering::SeqCst);
        let fs = make_test_fs_with_device(128, device.clone());
        let zeros = [0; DIRECT_RANGE_ZERO_CHUNK_BLOCKS * BLOCK_SIZE];

        let error = fs
            .initialize_allocated_range(32, DIRECT_RANGE_ZERO_CHUNK_BLOCKS * 2, &zeros)
            .unwrap_err();

        assert_eq!(error.code(), ErrCode::ENOMEM);
        assert_eq!(device.writes.load(Ordering::SeqCst), 2);
        assert_eq!(device.flushes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn allocated_range_flush_failure_is_reported_after_all_zero_chunks() {
        let device = Arc::new(RangeInitDevice::new(128));
        device.fail_flush.store(true, Ordering::SeqCst);
        let fs = make_test_fs_with_device(128, device.clone());
        let zeros = [0; DIRECT_RANGE_ZERO_CHUNK_BLOCKS * BLOCK_SIZE];

        let error = fs
            .initialize_allocated_range(32, DIRECT_RANGE_ZERO_CHUNK_BLOCKS * 2, &zeros)
            .unwrap_err();

        assert_eq!(error.code(), ErrCode::EIO);
        assert_eq!(device.writes.load(Ordering::SeqCst), 2);
        assert_eq!(device.flushes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn transactional_range_plan_filters_legacy_writes_before_transaction_gate() {
        let fs = make_test_fs(1024);
        let mut inode = Inode::default();
        inode.extent_init();
        let mut inode = InodeRef::new(2, Box::new(inode));

        assert!(fs
            .transactional_range_plan(&inode, 0, (DIRECT_RANGE_MIN_BLOCKS - 1) * BLOCK_SIZE)
            .unwrap()
            .is_none());
        assert!(fs
            .transactional_range_plan(&inode, 0, (DIRECT_RANGE_MAX_BLOCKS + 1) * BLOCK_SIZE)
            .unwrap()
            .is_none());
        assert!(fs
            .transactional_range_plan(&inode, 0, DIRECT_RANGE_MIN_BLOCKS * BLOCK_SIZE)
            .unwrap()
            .is_some());

        inode
            .inode
            .set_size((DIRECT_RANGE_MIN_BLOCKS * BLOCK_SIZE) as u64);
        assert!(fs
            .transactional_range_plan(&inode, 0, DIRECT_RANGE_MIN_BLOCKS * BLOCK_SIZE)
            .unwrap()
            .is_none());
    }

    #[test]
    fn transactional_range_plan_trims_one_mapped_prefix_block() {
        let fs = make_test_fs(1024);
        let mut inode = Inode::default();
        inode.extent_init();
        let mut inode = InodeRef::new(2, Box::new(inode));
        fs.stage_direct_append_extent(&mut inode, 0, 100, 16)
            .unwrap();

        let plan = fs
            .transactional_range_plan(&inode, 15 * BLOCK_SIZE + 3360, 16 * BLOCK_SIZE)
            .unwrap()
            .expect("mapped prefix must not force per-block allocation");

        assert_eq!(plan.start_lblock, 16);
        assert_eq!(plan.count, 16);
        assert_eq!(plan.preferred_first, Some(116));
    }

    #[test]
    fn read_extent_or_hole_zero_fills_only_missing_extent() {
        let fs = make_test_fs(16);
        let mut inode = Inode::default();
        inode.extent_init();
        let inode = InodeRef::new(2, Box::new(inode));
        let mut buf = [0x5a; 16];

        fs.read_extent_or_hole(&inode, 0, 0, &mut buf).unwrap();

        assert_eq!(buf, [0; 16]);
    }

    #[test]
    fn metadata_mutation_barrier_separates_direct_and_transactional_writers() {
        let fs = make_test_fs(16);
        let waker = Arc::new(CountingMetadataWaker {
            wakes: AtomicUsize::new(0),
        });
        fs.install_metadata_mutation_waker(waker.clone()).unwrap();

        let direct = fs.lock_direct_metadata_mutation().unwrap();
        let second_direct = fs.lock_direct_metadata_mutation().unwrap();
        assert_eq!(
            fs.lock_transactional_metadata_mutation()
                .expect_err("exclusive gate must not wait for direct owners")
                .code(),
            ErrCode::EAGAIN
        );
        drop(second_direct);
        assert_eq!(fs.metadata_mutation_generation(), 0);
        assert_eq!(waker.wakes.load(Ordering::SeqCst), 0);
        drop(direct);
        assert_eq!(fs.metadata_mutation_generation(), 1);
        assert_eq!(waker.wakes.load(Ordering::SeqCst), 1);

        let transaction = fs.lock_transactional_metadata_mutation().unwrap();
        assert_eq!(
            fs.lock_direct_metadata_mutation()
                .expect_err("direct gate must not wait for exclusive owner")
                .code(),
            ErrCode::EAGAIN
        );
        assert_eq!(
            fs.lock_transactional_metadata_mutation()
                .expect_err("second exclusive owner must be rejected")
                .code(),
            ErrCode::EAGAIN
        );
        drop(transaction);
        assert_eq!(fs.metadata_mutation_generation(), 2);
        assert_eq!(waker.wakes.load(Ordering::SeqCst), 2);
        drop(fs.lock_transactional_metadata_mutation().unwrap());
        drop(fs.lock_direct_metadata_mutation().unwrap());
        assert_eq!(fs.metadata_mutation_generation(), 4);
        assert_eq!(waker.wakes.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn metadata_mutation_barrier_rejects_direct_count_overflow() {
        let fs = make_test_fs(16);
        fs.metadata_mutation_barrier.state.store(
            crate::ext4::METADATA_GATE_DIRECT_MAX,
            core::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            fs.lock_direct_metadata_mutation()
                .expect_err("direct count must not enter the exclusive bit")
                .code(),
            ErrCode::EIO
        );
        fs.metadata_mutation_barrier
            .state
            .store(0, core::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn metadata_mutation_generation_wrap_and_fail_stop_are_observable() {
        let fs = make_test_fs(16);
        let waker = Arc::new(CountingMetadataWaker {
            wakes: AtomicUsize::new(0),
        });
        fs.install_metadata_mutation_waker(waker.clone()).unwrap();
        fs.metadata_mutation_barrier
            .generation
            .store(u64::MAX, Ordering::Relaxed);

        drop(fs.lock_transactional_metadata_mutation().unwrap());
        assert_eq!(fs.metadata_mutation_generation(), 0);
        assert_eq!(waker.wakes.load(Ordering::SeqCst), 1);

        fs.fail_stop_mutations();
        assert!(fs.metadata_mutations_terminal());
        assert_eq!(fs.metadata_mutation_generation(), 1);
        assert_eq!(waker.wakes.load(Ordering::SeqCst), 2);
        fs.fail_stop_mutations();
        assert_eq!(fs.metadata_mutation_generation(), 1);
        assert_eq!(waker.wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn metadata_mutation_waker_is_install_once() {
        let fs = make_test_fs(16);
        let first = Arc::new(CountingMetadataWaker {
            wakes: AtomicUsize::new(0),
        });
        fs.install_metadata_mutation_waker(first.clone()).unwrap();
        assert_eq!(
            fs.install_metadata_mutation_waker(Arc::new(CountingMetadataWaker {
                wakes: AtomicUsize::new(0),
            }))
            .unwrap_err()
            .code(),
            ErrCode::EINVAL
        );
        // Re-installing even the same object is a caller lifecycle error.
        assert_eq!(
            fs.install_metadata_mutation_waker(first)
                .unwrap_err()
                .code(),
            ErrCode::EINVAL
        );
    }

    #[test]
    fn transaction_wrapper_fail_stops_raw_core_writer_collision() {
        let fs = make_test_fs(16);
        let MetadataMutationMode::Direct(core) = &fs.metadata_mode else {
            panic!("fixture must use the direct transaction core");
        };
        let owner = core.start(1).unwrap();

        let error = match fs.transaction_start(1) {
            Err(error) => error,
            Ok(_) => panic!("core-writer collision must fail-stop"),
        };
        assert_eq!(error.code(), ErrCode::EIO);
        assert!(fs.metadata_mutations_terminal());
        owner.abort();
    }

    #[test]
    fn direct_range_wrapper_fail_stops_raw_core_writer_collision() {
        let fs = make_test_fs(16);
        let MetadataMutationMode::Direct(core) = &fs.metadata_mode else {
            panic!("fixture must use the direct transaction core");
        };
        let owner = core.start(1).unwrap();

        let error = match fs.transaction_start_direct_range(1) {
            Err(error) => error,
            Ok(_) => panic!("direct-range core-writer collision must fail-stop"),
        };
        assert_eq!(error.code(), ErrCode::EIO);
        assert!(fs.metadata_mutations_terminal());
        owner.abort();
    }

    #[test]
    fn metadata_mutation_barrier_allows_concurrent_direct_owners() {
        let fs = make_test_fs(16);
        let start = std::sync::Barrier::new(3);
        let release = std::sync::Barrier::new(3);
        let (sender, receiver) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            for _ in 0..2 {
                let sender = sender.clone();
                let start = &start;
                let release = &release;
                let fs = &fs;
                scope.spawn(move || {
                    start.wait();
                    let guard = fs.lock_direct_metadata_mutation();
                    sender.send(guard.is_ok()).unwrap();
                    release.wait();
                    drop(guard);
                });
            }
            start.wait();
            assert!(receiver.recv().unwrap());
            assert!(receiver.recv().unwrap());
            assert_eq!(
                fs.lock_transactional_metadata_mutation()
                    .expect_err("both direct guards must remain live")
                    .code(),
                ErrCode::EAGAIN
            );
            release.wait();
        });
        drop(fs.lock_transactional_metadata_mutation().unwrap());
    }

    #[test]
    fn clean_delalloc_ledger_mutation_completes_before_concurrent_fail_stop() {
        let fs = Arc::new(make_test_fs(16));
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(0);

        let mut lease = std::thread::scope(|scope| {
            let writer_fs = fs.clone();
            let writer = scope.spawn(move || {
                writer_fs.test_reserve_clean_delalloc_lease_with_hook(|| {
                    // The helper has both `alloc_lock` and `poisoned` here.
                    // Hold that exact linearization point until fail-stop has
                    // been started on another thread.
                    entered_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                })
            });

            entered_rx.recv().unwrap();
            let poison_fs = fs.clone();
            let poisoner = scope.spawn(move || poison_fs.fail_stop_mutations());
            continue_tx.send(()).unwrap();

            let lease = writer
                .join()
                .expect("ledger writer must not panic")
                .expect("pre-admitted ledger update must win the race");
            poisoner
                .join()
                .expect("fail-stop must not wait on alloc_lock");
            lease
        });

        // The winning reservation is ordered before poison, but abandonment
        // after poison must leave its capacity unavailable for this mount.
        fs.abandon_delalloc_lease_after_fail_stop(&mut lease)
            .expect("post-poison owner must abandon the pre-admitted lease");
        let allocation = fs.alloc_lock.lock();
        assert_eq!(allocation.reserved_data_blocks, 1);
        assert_eq!(allocation.delalloc_claims.len(), 1);
    }

    #[test]
    fn clean_delalloc_ledger_mutation_rejects_after_fail_stop_without_releasing_capacity() {
        let fs = make_test_fs(16);
        let mut lease = fs
            .test_reserve_clean_delalloc_lease_with_hook(|| {})
            .expect("test fixture must create one in-memory lease");

        fs.fail_stop_mutations();
        assert_eq!(
            fs.test_release_clean_delalloc_lease(&mut lease, || {})
                .expect_err("poison must win before a later ledger release")
                .code(),
            ErrCode::EIO
        );
        assert!(lease.active);
        fs.abandon_delalloc_lease_after_fail_stop(&mut lease)
            .expect("failed post-poison release must terminalise by abandonment");

        let allocation = fs.alloc_lock.lock();
        assert_eq!(allocation.reserved_data_blocks, 1);
        assert_eq!(allocation.delalloc_claims.len(), 1);
    }

    #[test]
    fn foreign_mapper_authority_never_terminalises_another_mount_reservation() {
        let fs_a = make_test_fs(16);
        let fs_b = make_test_fs(16);
        let authority_a = DelallocAppendMapperAuthority {
            mount_generation: fs_a.delalloc_mount_generation(),
        };
        let lease_b = fs_b
            .test_reserve_clean_delalloc_lease_with_hook(|| {})
            .expect("fixture B must reserve one exact lease");
        let mut reservation_b = DelallocAppendBlockReservation::new(lease_b);

        let outcome = fs_a.submit_delalloc_append_block_authorized(
            &authority_a,
            &mut reservation_b,
            &[0u8; BLOCK_SIZE],
            BLOCK_SIZE as u64,
            None,
        );
        assert_eq!(
            outcome,
            DelallocAppendBlockSubmitOutcome::RetryableNotPublished(ErrCode::EINVAL)
        );
        assert!(reservation_b.lease.active);

        fs_a.fail_stop_mutations();
        assert_eq!(
            fs_a.terminalize_delalloc_append_block_authorized_after_fail_stop(
                &authority_a,
                &mut reservation_b,
            )
            .unwrap_err()
            .code(),
            ErrCode::EINVAL
        );
        assert!(reservation_b.lease.active);
        fs_b.test_release_clean_delalloc_lease(&mut reservation_b.lease, || {})
            .expect("source mount must retain normal release authority");
    }

    #[test]
    fn fail_stop_while_holding_direct_gate_and_allocator_does_not_deadlock_lease_release() {
        let fs = Arc::new(make_test_fs(16));
        let mut lease = fs
            .test_reserve_clean_delalloc_lease_with_hook(|| {})
            .expect("test fixture must create one in-memory lease");
        let (holder_ready_tx, holder_ready_rx) = std::sync::mpsc::sync_channel(0);
        let (poison_now_tx, poison_now_rx) = std::sync::mpsc::sync_channel(0);
        let (release_started_tx, release_started_rx) = std::sync::mpsc::sync_channel(0);

        let release_result = {
            let lease_ref = &mut lease;
            std::thread::scope(|scope| {
                let holder_fs = fs.clone();
                let holder = scope.spawn(move || {
                    let _direct = holder_fs
                        .lock_direct_metadata_mutation()
                        .expect("direct fixture gate must admit its first holder");
                    let _allocation = holder_fs.alloc_lock.lock();
                    holder_ready_tx.send(()).unwrap();
                    poison_now_rx.recv().unwrap();
                    // This is the historical ABBA trigger: fail-stop is invoked
                    // while the direct gate and allocator are both owned.
                    holder_fs.fail_stop_mutations();
                });

                holder_ready_rx.recv().unwrap();
                let release_fs = fs.clone();
                let releaser = scope.spawn(move || {
                    let _direct = release_fs
                        .lock_direct_metadata_mutation()
                        .expect("direct writers must remain mutually compatible");
                    release_started_tx.send(()).unwrap();
                    release_fs.test_release_clean_delalloc_lease(lease_ref, || {})
                });

                release_started_rx.recv().unwrap();
                poison_now_tx.send(()).unwrap();
                holder.join().expect("fail-stop must not self-deadlock");
                releaser
                    .join()
                    .expect("post-poison lease release must return")
            })
        };

        assert_eq!(
            release_result
                .expect_err("release after fail-stop must not restore capacity")
                .code(),
            ErrCode::EIO
        );
        assert!(lease.active);
        fs.abandon_delalloc_lease_after_fail_stop(&mut lease)
            .expect("post-poison release owner must abandon its live lease");
    }

    #[test]
    fn clean_delalloc_release_linearizes_before_a_waiting_fail_stop() {
        let fs = Arc::new(make_test_fs(16));
        let mut lease = fs
            .test_reserve_clean_delalloc_lease_with_hook(|| {})
            .expect("test fixture must create one in-memory lease");
        let (release_entered_tx, release_entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_continue_tx, release_continue_rx) = std::sync::mpsc::sync_channel(0);
        let (poison_requested_tx, poison_requested_rx) = std::sync::mpsc::sync_channel(0);

        std::thread::scope(|scope| {
            let release_fs = fs.clone();
            let lease_ref = &mut lease;
            let releaser = scope.spawn(move || {
                release_fs.test_release_clean_delalloc_lease(lease_ref, || {
                    release_entered_tx.send(()).unwrap();
                    release_continue_rx.recv().unwrap();
                })
            });

            release_entered_rx.recv().unwrap();
            let poison_fs = fs.clone();
            let poisoner = scope.spawn(move || {
                poison_fs.test_fail_stop_mutations_before_poison_lock(|| {
                    poison_requested_tx.send(()).unwrap();
                });
            });

            // The release hook still holds both `alloc_lock` and `poisoned`.
            // Therefore this signal proves the fail-stop thread is about to
            // contend on the latter lock, but cannot have won it yet.
            poison_requested_rx.recv().unwrap();
            release_continue_tx.send(()).unwrap();
            releaser
                .join()
                .expect("release thread must not panic")
                .expect("release must linearize before the waiting fail-stop");
            poisoner
                .join()
                .expect("fail-stop must complete after release");
        });

        assert!(!lease.active);
        let allocation = fs.alloc_lock.lock();
        assert_eq!(allocation.reserved_data_blocks, 0);
        assert!(allocation.delalloc_claims.is_empty());
    }

    #[test]
    fn fail_stop_inside_transactional_gate_does_not_self_deadlock_or_reopen_release() {
        let fs = make_test_fs(16);
        let mut lease = fs
            .test_reserve_clean_delalloc_lease_with_hook(|| {})
            .expect("test fixture must create one in-memory lease");

        let transaction = fs
            .lock_transactional_metadata_mutation()
            .expect("test fixture must admit its transaction owner");
        fs.fail_stop_mutations();
        assert_eq!(
            fs.lock_direct_metadata_mutation()
                .expect_err("transaction gate must still reject a direct release owner")
                .code(),
            ErrCode::EAGAIN
        );
        drop(transaction);

        assert_eq!(
            fs.test_release_clean_delalloc_lease(&mut lease, || {})
                .expect_err("release after the transaction-gate fail-stop must be rejected")
                .code(),
            ErrCode::EIO
        );
        fs.abandon_delalloc_lease_after_fail_stop(&mut lease)
            .expect("post-transaction fail-stop owner must abandon its lease");
    }

    #[test]
    fn read_extent_or_hole_propagates_extent_corruption() {
        let fs = make_test_fs(16);
        let inode = InodeRef::new(2, Box::new(Inode::default()));
        let mut buf = [0x5a; 16];

        let err = fs
            .read_extent_or_hole(&inode, 0, 0, &mut buf)
            .expect_err("invalid extent root must not be treated as a hole");

        assert_eq!(err.code(), ErrCode::EIO);
        assert_eq!(buf, [0x5a; 16]);
    }

    const TEST_BLOCK_COUNT: usize = 16;
    const TEST_BLOCK_BITMAP: PBlockId = 2;
    const TEST_INODE_BITMAP: PBlockId = 3;
    const TEST_INODE_TABLE: PBlockId = 4;
    const TEST_XATTR_BLOCK: PBlockId = 5;
    const TEST_INITIAL_FREE_BLOCKS: u64 = (TEST_BLOCK_COUNT as u64) - 5;

    struct FailingBlockDevice {
        blocks: spin::Mutex<BTreeMap<PBlockId, Block>>,
        fail_reads: spin::Mutex<Vec<PBlockId>>,
        fail_writes: spin::Mutex<Vec<PBlockId>>,
    }

    impl FailingBlockDevice {
        fn new() -> Self {
            let mut blocks = BTreeMap::new();
            for block_id in 0..TEST_BLOCK_COUNT as PBlockId {
                blocks.insert(block_id, Block::new(block_id, Box::new([0u8; BLOCK_SIZE])));
            }

            let mut sb_block = blocks.remove(&0).unwrap();
            Self::write_u32(&mut sb_block, BASE_OFFSET, 16);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 4, TEST_BLOCK_COUNT as u32);
            Self::write_u32(
                &mut sb_block,
                BASE_OFFSET + 12,
                TEST_INITIAL_FREE_BLOCKS as u32,
            );
            Self::write_u32(&mut sb_block, BASE_OFFSET + 16, 15);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 20, 0);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 24, 2);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 28, 2);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 32, TEST_BLOCK_COUNT as u32);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 36, TEST_BLOCK_COUNT as u32);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 40, 16);
            Self::write_u16(&mut sb_block, BASE_OFFSET + 56, 0xef53);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 84, 1);
            Self::write_u16(&mut sb_block, BASE_OFFSET + 88, SB_GOOD_INODE_SIZE as u16);
            Self::write_u16(&mut sb_block, BASE_OFFSET + 254, SB_GOOD_DESC_SIZE as u16);
            blocks.insert(0, sb_block);

            let mut bgdt = blocks.remove(&1).unwrap();
            Self::write_u32(&mut bgdt, 0, TEST_BLOCK_BITMAP as u32);
            Self::write_u32(&mut bgdt, 4, TEST_INODE_BITMAP as u32);
            Self::write_u32(&mut bgdt, 8, TEST_INODE_TABLE as u32);
            Self::write_u16(&mut bgdt, 12, TEST_INITIAL_FREE_BLOCKS as u16);
            Self::write_u16(&mut bgdt, 14, 15);
            blocks.insert(1, bgdt);

            let mut bitmap = blocks.remove(&TEST_BLOCK_BITMAP).unwrap();
            bitmap.data[0] = 0b0001_1111;
            blocks.insert(TEST_BLOCK_BITMAP, bitmap);

            let mut inode_bitmap = blocks.remove(&TEST_INODE_BITMAP).unwrap();
            inode_bitmap.data[0] = 0b0000_0010;
            blocks.insert(TEST_INODE_BITMAP, inode_bitmap);

            let mut inode_table = blocks.remove(&TEST_INODE_TABLE).unwrap();
            let mut inode = Inode::default();
            inode.set_mode(InodeMode::from_type_and_perm(
                FileType::RegularFile,
                InodeMode::from_bits_retain(0o644),
            ));
            inode.set_link_count(1);
            inode_table.write_offset_as(SB_GOOD_INODE_SIZE, &inode);
            blocks.insert(TEST_INODE_TABLE, inode_table);

            Self {
                blocks: spin::Mutex::new(blocks),
                fail_reads: spin::Mutex::new(Vec::new()),
                fail_writes: spin::Mutex::new(Vec::new()),
            }
        }

        fn write_u16(block: &mut Block, offset: usize, value: u16) {
            block.write_offset(offset, &value.to_le_bytes());
        }

        fn write_u32(block: &mut Block, offset: usize, value: u32) {
            block.write_offset(offset, &value.to_le_bytes());
        }

        fn fail_once_on_read(&self, block_id: PBlockId) {
            self.fail_reads.lock().push(block_id);
        }

        fn fail_once_on_write(&self, block_id: PBlockId) {
            self.fail_writes.lock().push(block_id);
        }

        fn take_failure(list: &mut Vec<PBlockId>, block_id: PBlockId) -> bool {
            if let Some(pos) = list.iter().position(|&id| id == block_id) {
                list.remove(pos);
                true
            } else {
                false
            }
        }

        fn block_bitmap_bit_is_set(&self, bit: usize) -> bool {
            let blocks = self.blocks.lock();
            let block = blocks.get(&TEST_BLOCK_BITMAP).unwrap();
            (block.data[bit / 8] & (1 << (bit % 8))) != 0
        }

        fn bg_free_blocks(&self) -> u64 {
            let blocks = self.blocks.lock();
            let block = blocks.get(&1).unwrap();
            u16::from_le_bytes(block.data[12..14].try_into().unwrap()) as u64
        }

        fn sb_free_blocks(&self) -> u64 {
            let blocks = self.blocks.lock();
            let block = blocks.get(&0).unwrap();
            u32::from_le_bytes(
                block.data[BASE_OFFSET + 12..BASE_OFFSET + 16]
                    .try_into()
                    .unwrap(),
            ) as u64
        }

        fn disk_inode_xattr_block(&self) -> PBlockId {
            let blocks = self.blocks.lock();
            let block = blocks.get(&TEST_INODE_TABLE).unwrap();
            let inode: Inode = block.read_offset_as(SB_GOOD_INODE_SIZE);
            inode.xattr_block()
        }

        fn fill_block(&self, block_id: PBlockId, byte: u8) {
            self.blocks
                .lock()
                .get_mut(&block_id)
                .unwrap()
                .data
                .fill(byte);
        }

        fn block_is_zero(&self, block_id: PBlockId) -> bool {
            self.blocks
                .lock()
                .get(&block_id)
                .unwrap()
                .data
                .iter()
                .all(|byte| *byte == 0)
        }
    }

    impl BlockDevice for FailingBlockDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            if Self::take_failure(&mut self.fail_reads.lock(), block_id) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            self.blocks
                .lock()
                .get(&block_id)
                .cloned()
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))
        }

        fn write_block(&self, block: &Block) -> Result<()> {
            if Self::take_failure(&mut self.fail_writes.lock(), block.id) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            self.blocks.lock().insert(block.id, block.clone());
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    fn load_failing_test_fs() -> (Arc<FailingBlockDevice>, Ext4) {
        let block_device = Arc::new(FailingBlockDevice::new());
        let mut fs = Ext4::load(block_device.clone()).unwrap();
        fs.initialize_direct().unwrap();
        (block_device, fs)
    }

    fn assert_xattr_alloc_rolled_back(fs: &Ext4, block_device: &FailingBlockDevice) {
        assert!(!block_device.block_bitmap_bit_is_set(TEST_XATTR_BLOCK as usize));
        assert_eq!(block_device.bg_free_blocks(), TEST_INITIAL_FREE_BLOCKS);
        assert_eq!(block_device.sb_free_blocks(), TEST_INITIAL_FREE_BLOCKS);
        assert_eq!(
            fs.read_block_group(0).unwrap().desc.get_free_blocks_count(),
            TEST_INITIAL_FREE_BLOCKS
        );
        assert_eq!(
            fs.read_super_block_cached().free_blocks_count(),
            TEST_INITIAL_FREE_BLOCKS
        );
        assert_eq!(block_device.disk_inode_xattr_block(), 0);
    }

    fn assert_allocation_state(
        fs: &Ext4,
        block_device: &FailingBlockDevice,
        allocated: bool,
        free_blocks: u64,
    ) {
        assert_eq!(
            block_device.block_bitmap_bit_is_set(TEST_XATTR_BLOCK as usize),
            allocated
        );
        assert_eq!(block_device.bg_free_blocks(), free_blocks);
        assert_eq!(block_device.sb_free_blocks(), free_blocks);
        assert_eq!(
            fs.read_block_group(0).unwrap().desc.get_free_blocks_count(),
            free_blocks
        );
        assert_eq!(
            fs.read_super_block_cached().free_blocks_count(),
            free_blocks
        );
    }

    #[test]
    fn setxattr_rolls_back_when_new_xattr_block_read_fails() {
        let (block_device, fs) = load_failing_test_fs();
        block_device.fail_once_on_read(TEST_XATTR_BLOCK);

        let err = fs
            .setxattr_with_flags(2, "user.rollback", b"value", false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn setxattr_rolls_back_when_new_xattr_block_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        block_device.fail_once_on_write(TEST_XATTR_BLOCK);

        let err = fs
            .setxattr_with_flags(2, "user.rollback", b"value", false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn setxattr_rolls_back_when_inode_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        block_device.fail_once_on_write(TEST_INODE_TABLE);

        let err = fs
            .setxattr_with_flags(2, "user.rollback", b"value", false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn setxattr_rolls_back_when_new_xattr_does_not_fit() {
        let (block_device, fs) = load_failing_test_fs();
        let value = vec![0x5au8; BLOCK_SIZE];

        let err = fs
            .setxattr_with_flags(2, "user.rollback", &value, false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::ENOSPC);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn block_group_cache_updates_only_after_disk_write_succeeds() {
        let (block_device, fs) = load_failing_test_fs();
        let mut bg = fs.read_block_group(0).unwrap();
        bg.desc.set_free_blocks_count(TEST_INITIAL_FREE_BLOCKS - 1);
        block_device.fail_once_on_write(1);

        let err = fs.write_block_group_with_csum(&mut bg).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_eq!(
            fs.read_block_group(0).unwrap().desc.get_free_blocks_count(),
            TEST_INITIAL_FREE_BLOCKS
        );
        assert_eq!(block_device.bg_free_blocks(), TEST_INITIAL_FREE_BLOCKS);
    }

    #[test]
    fn alloc_block_rolls_back_when_block_group_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fail_once_on_write(1);

        let err = fs.alloc_block(&mut inode).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, false, TEST_INITIAL_FREE_BLOCKS);
    }

    #[test]
    fn alloc_block_rolls_back_when_superblock_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fail_once_on_write(0);

        let err = fs.alloc_block(&mut inode).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, false, TEST_INITIAL_FREE_BLOCKS);
    }

    #[test]
    fn nojournal_mount_rejects_delayed_reservation_without_restricting_eager_allocation() {
        fn assert_send<T: Send>() {}

        assert_send::<DelallocAppendBlockReservation>();
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();

        assert_eq!(
            fs.reserve_delalloc_lease(1, 0).unwrap_err().code(),
            ErrCode::ENOTSUP
        );
        // This call exercises the normal-build append capability boundary,
        // rather than the host-only raw lease facade.  A no-journal mount
        // must reject both before it can hand a live capability to VFS.
        assert_eq!(
            fs.reserve_delalloc_append_block_capability(2, 0)
                .unwrap_err()
                .code(),
            ErrCode::ENOTSUP
        );
        assert_eq!(fs.alloc_block(&mut inode).unwrap(), TEST_XATTR_BLOCK);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
    }

    #[test]
    fn newly_reused_data_block_is_zeroed_before_mapping() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fill_block(TEST_XATTR_BLOCK, 0xa5);

        let pblock = fs.alloc_zeroed_data_block(&mut inode).unwrap();

        assert_eq!(pblock, TEST_XATTR_BLOCK);
        assert!(block_device.block_is_zero(pblock));
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
    }

    #[test]
    fn data_block_zero_write_failure_rolls_back_allocation() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fill_block(TEST_XATTR_BLOCK, 0xa5);
        block_device.fail_once_on_write(TEST_XATTR_BLOCK);

        let err = fs.alloc_zeroed_data_block(&mut inode).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, false, TEST_INITIAL_FREE_BLOCKS);
        assert!(!block_device.block_is_zero(TEST_XATTR_BLOCK));
    }

    #[test]
    fn unpublished_extent_rollback_uses_the_tree_before_i_blocks_is_recomputed() {
        let (block_device, fs) = load_failing_test_fs();
        let initial_sb_free_inodes = fs.read_super_block_cached().free_inodes_count();
        let initial_bg_free_inodes = fs.read_block_group(0).unwrap().desc.free_inodes_count();
        let mut inode = fs
            .create_inode_with_owner(
                InodeMode::SOFTLINK | InodeMode::ALL_RWX,
                0,
                0,
            )
            .unwrap();
        assert!(fs.inode_is_allocated(inode.id).unwrap());
        block_device.fail_once_on_write(TEST_INODE_TABLE);

        let err = fs
            .extent_query_or_create_initialized(
                &mut inode,
                0,
                1,
                Some(Box::new([0x5a; BLOCK_SIZE])),
            )
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_eq!(inode.inode.fs_block_count(), 0);
        assert_eq!(fs.extent_all_data_blocks(&inode).unwrap(), vec![TEST_XATTR_BLOCK]);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);

        fs.free_inode(&mut inode).unwrap();
        assert_allocation_state(&fs, &block_device, false, TEST_INITIAL_FREE_BLOCKS);
        assert!(!fs.inode_is_allocated(inode.id).unwrap());
        assert_eq!(
            fs.read_super_block_cached().free_inodes_count(),
            initial_sb_free_inodes
        );
        assert_eq!(
            fs.read_block_group(0).unwrap().desc.free_inodes_count(),
            initial_bg_free_inodes
        );
    }

    #[test]
    fn dealloc_block_rolls_back_when_block_group_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        let pblock = fs.alloc_block(&mut inode).unwrap();
        assert_eq!(pblock, TEST_XATTR_BLOCK);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
        block_device.fail_once_on_write(1);

        let err = fs.dealloc_block(&mut inode, pblock).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
    }

    #[test]
    fn dealloc_block_rolls_back_when_superblock_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        let pblock = fs.alloc_block(&mut inode).unwrap();
        assert_eq!(pblock, TEST_XATTR_BLOCK);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
        block_device.fail_once_on_write(0);

        let err = fs.dealloc_block(&mut inode, pblock).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
    }
}
