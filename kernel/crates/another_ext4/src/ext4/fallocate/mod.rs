//! Bounded physical range mutations. No user i_size, cache or credential policy
//! lives here. Journal/Batched share private-image algorithms; Direct ordered
//! publication is isolated from their in-place checkpoint protocol.
mod direct;
use super::{journal_transaction::CommitFailure, rw::MetadataIo, Ext4};
use crate::{constants::*, ext4_defs::*, prelude::*};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreallocationProgress {
    pub next_offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExistingBlockImageOutcome {
    Written,
    LogicallyZero,
}

impl Ext4 {
    fn validate_range_inode(&self, inode: &InodeRef) -> Result<()> {
        if !inode.inode.is_file() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if !inode.inode.uses_extents() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        Ok(())
    }

    pub(super) fn range_transaction_credits(
        &self,
        inode: &InodeRef,
        splice: bool,
    ) -> Result<usize> {
        let depth = usize::from(inode.inode.extent_root().header().depth());
        // One insertion is bounded by the touched path and one new sibling per
        // level plus root growth. Splice can insert twice and retire a depth-
        // bounded chain whose bitmap/GDT homes may be in distinct groups.
        let credits = if splice {
            depth
                .checked_mul(13)
                .and_then(|value| value.checked_add(35))
        } else {
            depth.checked_mul(5).and_then(|value| value.checked_add(16))
        }
        .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        if !self.transaction_credits_fit(credits)? {
            return Err(Ext4Error::new(ErrCode::ENOSPC));
        }
        Ok(credits)
    }

    pub(super) fn finish_range_transaction(
        &self,
        transaction: super::journal_transaction::Transaction<'_>,
    ) -> Result<()> {
        self.commit_metadata_operation(transaction)
            .map(|_| ())
            .map_err(|error| {
                if error.poisoned || error.failure != CommitFailure::BeforeCommit {
                    self.poison(ErrCode::EIO);
                }
                error.error
            })
    }

    /// Advance through an existing allocation or allocate exactly one missing
    /// physical block. Return only after publication and after releasing locks.
    pub fn preallocate_range_batch(
        &self,
        id: InodeId,
        offset: usize,
        len: usize,
    ) -> Result<PreallocationProgress> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        if len == 0 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if (end - 1) / BLOCK_SIZE >= MAX_BLOCKS as usize {
            return Err(Ext4Error::new(ErrCode::EFBIG));
        }
        self.ensure_mutable()?;
        let _metadata = self.lock_transactional_metadata_mutation()?;
        let _inode = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode_uncached(id)?;
        self.validate_range_inode(&inode)?;
        if !self.uses_journal() {
            return self.direct_preallocate_range_batch_locked(&mut inode, offset, len);
        }
        let logical = (offset / BLOCK_SIZE) as LBlockId;
        if let Some(extent) = self.allocated_extent_at_or_after(&inode, logical, None)? {
            if extent.start_lblock() <= logical {
                let next = extent
                    .start_lblock()
                    .checked_add(extent.block_count())
                    .and_then(|value| (value as usize).checked_mul(BLOCK_SIZE))
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                return Ok(PreallocationProgress {
                    next_offset: next.min(end),
                });
            }
        }
        let mut transaction =
            self.transaction_start(self.range_transaction_credits(&inode, false)?)?;
        let home = MetadataIo::transaction(self, &mut transaction)
            .allocate_initialized_data(&mut inode, Box::new([0; BLOCK_SIZE]))?;
        // Ext4 permits KEEP allocations beyond i_size as unwritten mappings.
        // Initialized mappings beyond EOF make e2fsck demand an enlarged size.
        let mut extent = Extent::new(logical, home, 1);
        extent.mark_unwritten();
        self.transaction_insert_allocated_extent(&mut transaction, &mut inode, &extent)?;
        self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
        self.finish_range_transaction(transaction)?;
        Ok(PreallocationProgress {
            next_offset: ((logical as usize + 1) * BLOCK_SIZE).min(end),
        })
    }

    /// Remove complete logical blocks. Each transaction frees one bounded
    /// fragment in one physical block group; earlier successful batches survive
    /// a later failure. The upper caller owns stable EOF and partial edges.
    pub fn punch_block_range(&self, id: InodeId, first: LBlockId, end: LBlockId) -> Result<()> {
        if first > end {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.ensure_mutable()?;
        let _metadata = self.lock_transactional_metadata_mutation()?;
        let _inode = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        self.punch_block_range_locked(id, first, end)
    }

    /// Truncate/recovery already owns both lower exclusions.
    pub(super) fn punch_block_range_locked(
        &self,
        id: InodeId,
        first: LBlockId,
        end: LBlockId,
    ) -> Result<()> {
        let mut cursor = first;
        while cursor < end {
            let mut inode = self.read_inode_uncached(id)?;
            self.validate_range_inode(&inode)?;
            let Some(extent) = self.allocated_extent_at_or_after(&inode, cursor, None)? else {
                break;
            };
            cursor = cursor.max(extent.start_lblock());
            if cursor >= end {
                break;
            }
            let physical = extent
                .start_pblock()
                .checked_add(u64::from(cursor - extent.start_lblock()))
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            let sb = self.read_super_block_cached();
            let group_size = u64::from(sb.blocks_per_group());
            let base = u64::from(sb.first_data_block());
            if group_size == 0 || physical < base {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            let group_left = group_size - (physical - base) % group_size;
            let extent_end = extent
                .start_lblock()
                .checked_add(extent.block_count())
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            let count = u64::from(extent_end.min(end) - cursor).min(group_left) as u32;
            let stop = cursor
                .checked_add(count)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            if !self.uses_journal() {
                self.direct_punch_extent_slice_locked(&mut inode, cursor, stop)?;
                cursor = stop;
                continue;
            }
            let mut transaction =
                self.transaction_start(self.range_transaction_credits(&inode, true)?)?;
            let removed = self.transaction_splice_allocated_extent(
                &mut transaction,
                &mut inode,
                cursor,
                stop,
                false,
            )?;
            self.transaction_dealloc_block_range(&mut transaction, physical, count)?;
            for home in &removed {
                self.transaction_dealloc_block_range(&mut transaction, *home, 1)?;
            }
            let released = u64::from(count) + removed.len() as u64;
            inode.inode.set_fs_block_count(
                inode
                    .inode
                    .fs_block_count()
                    .checked_sub(released)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
            );
            self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
            self.finish_range_transaction(transaction)?;
            cursor = stop;
        }
        Ok(())
    }

    /// Persist a caller-prepared full image only into an existing initialized
    /// mapping. Neither logical-zero coverage nor the inode size is changed.
    pub fn write_existing_block_image(
        &self,
        id: InodeId,
        logical: LBlockId,
        image: &[u8; BLOCK_SIZE],
    ) -> Result<ExistingBlockImageOutcome> {
        if logical == MAX_BLOCKS {
            return Err(Ext4Error::new(ErrCode::EFBIG));
        }
        self.ensure_mutable()?;
        let _metadata = self.lock_transactional_metadata_mutation()?;
        let _inode = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let inode = self.read_inode_uncached(id)?;
        self.validate_range_inode(&inode)?;
        let Some(extent) = self.allocated_extent_at_or_after(&inode, logical, None)? else {
            return Ok(ExistingBlockImageOutcome::LogicallyZero);
        };
        if extent.start_lblock() > logical || extent.is_unwritten() {
            return Ok(ExistingBlockImageOutcome::LogicallyZero);
        }
        if !self.block_device.supports_reliable_flush() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let home = extent
            .start_pblock()
            .checked_add(u64::from(logical - extent.start_lblock()))
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        self.block_device
            .write_block(&Block::new(home, Box::new(*image)))?;
        // An error cannot undo payload bytes. The caller must not publish SIZE
        // on error; no metadata/bitmap publication occurred in this operation.
        self.block_device.flush()?;
        Ok(ExistingBlockImageOutcome::Written)
    }
}
