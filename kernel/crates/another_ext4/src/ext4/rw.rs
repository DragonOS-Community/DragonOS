use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::prelude::*;

impl Ext4 {
    /// Obtain a transaction-private metadata block image for mutation.
    /// Repeated calls for the same block merge changes in one staged image.
    pub(super) fn transaction_block_for_update<'tx>(
        &self,
        transaction: &'tx mut super::journal_transaction::Transaction<'_>,
        block_id: PBlockId,
    ) -> Result<&'tx mut [u8; BLOCK_SIZE]> {
        transaction.read_for_update(self.block_device.as_ref(), block_id)
    }

    /// Stage block 0 with a checksum-correct ext4 superblock while preserving
    /// boot-sector bytes and every unrelated field in the containing block.
    pub(super) fn transaction_stage_super_block(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        sb: &SuperBlock,
    ) -> Result<()> {
        let image = self.transaction_block_for_update(transaction, 0)?;
        let mut checksummed = *sb;
        checksummed.set_checksum();
        image[BASE_OFFSET..BASE_OFFSET + core::mem::size_of::<SuperBlock>()]
            .copy_from_slice(checksummed.to_bytes());
        Ok(())
    }

    /// Read the transaction's current superblock image.  This must be used by
    /// operations which may update allocation counters more than once in one
    /// transaction; the mounted cache is deliberately not published until the
    /// checkpoint is durable.
    pub(super) fn transaction_read_super_block(
        &self,
        transaction: &super::journal_transaction::Transaction<'_>,
    ) -> Result<SuperBlock> {
        let image = transaction.read(self.block_device.as_ref(), 0)?;
        Ok(SuperBlock::from_bytes(&image[BASE_OFFSET..]))
    }

    /// Read a block-group descriptor from the transaction's current image.
    pub(super) fn transaction_read_block_group(
        &self,
        transaction: &super::journal_transaction::Transaction<'_>,
        block_group_id: BlockGroupId,
    ) -> Result<BlockGroupRef> {
        let (block_id, offset) = self.block_group_disk_pos(block_group_id)?;
        let image = transaction.read(self.block_device.as_ref(), block_id)?;
        Ok(BlockGroupRef::new(
            block_group_id,
            BlockGroupDesc::from_bytes(&image[offset..]),
        ))
    }

    /// Merge a checksum-correct descriptor into its transaction-owned GDT
    /// block.  Several groups may share that block, so staging a fresh block
    /// for each descriptor would lose earlier changes.
    pub(super) fn transaction_stage_block_group_with_csum(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        bg_ref: &mut BlockGroupRef,
    ) -> Result<()> {
        bg_ref.set_checksum(self.read_super_block_cached().metadata_checksum_seed());
        let (block_id, offset) = self.block_group_disk_pos(bg_ref.id)?;
        let image = self.transaction_block_for_update(transaction, block_id)?;
        image[offset..offset + core::mem::size_of::<BlockGroupDesc>()]
            .copy_from_slice(bg_ref.desc.to_bytes());
        Ok(())
    }

    /// Stage a checksum-correct inode-table entry without publishing it to the
    /// value cache before the surrounding journal transaction commits.
    ///
    /// Using the transaction-owned block image is important because several
    /// inodes may share one inode-table block; repeated calls must merge rather
    /// than overwrite an earlier staged entry.
    pub(super) fn transaction_stage_inode_with_csum(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode_ref: &mut InodeRef,
    ) -> Result<()> {
        inode_ref.set_checksum(self.read_super_block_cached().metadata_checksum_seed());
        let (block_id, offset) = self.inode_disk_pos(inode_ref.id)?;
        let image = self.transaction_block_for_update(transaction, block_id)?;
        let bytes = inode_ref.inode.to_bytes();
        image[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// Read a block from block device
    pub(super) fn read_block(&self, block_id: PBlockId) -> Result<Block> {
        self.block_device.read_block(block_id)
    }

    /// Write a block to block device
    pub(super) fn write_block(&self, block: &Block) -> Result<()> {
        self.block_device.write_block(block)
    }

    /// Read super block from in-memory cache (no disk I/O).
    pub(super) fn read_super_block_cached(&self) -> SuperBlock {
        *self.cached_super_block.lock()
    }

    /// Read super block from block device (bypasses cache).
    #[allow(unused)]
    pub(super) fn read_super_block(&self) -> Result<SuperBlock> {
        Ok(self.read_super_block_cached())
    }

    /// Write super block to block device and update cache.
    ///
    /// The disk write is performed **before** the cache update so that a
    /// failed I/O does not leave the in-memory cache diverged from on-disk
    /// state (which would corrupt free-block / free-inode accounting).
    pub(super) fn write_super_block(&self, sb: &SuperBlock) -> Result<()> {
        // Write to disk first.
        // Must preserve other content in block 0 besides superblock (such as 1024B padding/boot code),
        // cannot overwrite with all-zero block; also need to update superblock checksum,
        // otherwise Linux will consider the filesystem corrupted and require e2fsck.
        let mut block = self.read_block(0)?;
        let mut sb_with_csum = *sb;
        sb_with_csum.set_checksum();
        block.write_offset_as(BASE_OFFSET, &sb_with_csum);
        self.write_block(&block)?;
        // Disk write succeeded, now commit to cache.
        *self.cached_super_block.lock() = *sb;
        Ok(())
    }

    /// Read an inode from cache or block device, return an `InodeRef` that
    /// combines the inode and its id.
    pub(super) fn read_inode(&self, inode_id: InodeId) -> Result<InodeRef> {
        // Try cache first
        if let Some(cached) = self.inode_cache.lock().get(inode_id) {
            return Ok(cached);
        }
        // Cache miss: read from disk
        let (block_id, offset) = self.inode_disk_pos(inode_id)?;
        let block = self.read_block(block_id)?;
        let inode_ref = InodeRef::new(inode_id, Box::new(block.read_offset_as(offset)));
        // Populate cache
        self.inode_cache.lock().insert(inode_ref.clone());
        Ok(inode_ref)
    }

    /// Read the authoritative inode-table entry without consulting the value cache.
    pub(super) fn read_inode_uncached(&self, inode_id: InodeId) -> Result<InodeRef> {
        let (block_id, offset) = self.inode_disk_pos(inode_id)?;
        let block = self.read_block(block_id)?;
        Ok(InodeRef::new(
            inode_id,
            Box::new(block.read_offset_as(offset)),
        ))
    }

    /// Read the root inode from block device
    #[allow(unused)]
    pub(super) fn read_root_inode(&self) -> Result<InodeRef> {
        self.read_inode(EXT4_ROOT_INO)
    }

    /// Write an inode to block device with checksum, and update cache.
    pub(super) fn write_inode_with_csum(&self, inode_ref: &mut InodeRef) -> Result<()> {
        let seed = self.read_super_block_cached().metadata_checksum_seed();
        inode_ref.set_checksum(seed);
        self.write_inode_without_csum(inode_ref)
    }

    /// Write an inode to block device without checksum, and update cache.
    pub(super) fn write_inode_without_csum(&self, inode_ref: &InodeRef) -> Result<()> {
        let (block_id, offset) = self.inode_disk_pos(inode_ref.id)?;
        let mut block = self.read_block(block_id)?;
        block.write_offset_as(offset, &*inode_ref.inode);
        self.write_block(&block)?;
        // Update cache with the latest inode data
        self.inode_cache.lock().update(inode_ref);
        Ok(())
    }

    /// Read a block group descriptor from cache, return a `BlockGroupRef`
    /// that combines the block group descriptor and its id.
    pub(super) fn read_block_group(&self, block_group_id: BlockGroupId) -> Result<BlockGroupRef> {
        let desc = *self
            .cached_block_groups
            .get(block_group_id as usize)
            .ok_or_else(|| {
                crate::format_error!(ErrCode::EINVAL, "Invalid block group id {}", block_group_id)
            })?
            .lock();
        Ok(BlockGroupRef::new(block_group_id, desc))
    }

    /// Write a block group descriptor to disk and update cache, with checksum.
    pub(super) fn write_block_group_with_csum(&self, bg_ref: &mut BlockGroupRef) -> Result<()> {
        let seed = self.read_super_block_cached().metadata_checksum_seed();
        bg_ref.set_checksum(seed);
        self.write_block_group_without_csum(bg_ref)
    }

    /// Write a block group descriptor to disk and update cache, without checksum.
    #[allow(unused)]
    pub(super) fn write_block_group_without_csum(&self, bg_ref: &BlockGroupRef) -> Result<()> {
        let sb = self.read_super_block_cached();
        let desc_per_block = BLOCK_SIZE as u32 / sb.desc_size() as u32;
        let block_id = sb.first_data_block() + bg_ref.id / desc_per_block + 1;
        let offset = (bg_ref.id % desc_per_block) * sb.desc_size() as u32;
        let mut block = self.read_block(block_id as PBlockId)?;
        block.write_offset_as(offset as usize, &bg_ref.desc);
        self.write_block(&block)?;
        if let Some(cached) = self.cached_block_groups.get(bg_ref.id as usize) {
            *cached.lock() = bg_ref.desc;
        }
        Ok(())
    }

    /// Get disk position of an inode. Return block id and offset within the block.
    ///
    /// Each block group contains `sb.inodes_per_group` inodes.
    /// Because inode 0 is defined not to exist, this formula can
    /// be used to find the block group that an inode lives in:
    /// `bg = (inode_id - 1) / sb.inodes_per_group`.
    ///
    /// The particular inode can be found within the block group's
    /// inode table at `index = (inode_id - 1) % sb.inodes_per_group`.
    /// To get the byte address within the inode table, use
    /// `offset = index * sb.inode_size`.
    pub(super) fn inode_disk_pos(&self, inode_id: InodeId) -> Result<(PBlockId, usize)> {
        let super_block = self.read_super_block_cached();
        let inodes_per_group = super_block.inodes_per_group();

        let bg_id = ((inode_id - 1) / inodes_per_group) as BlockGroupId;
        let inode_size = super_block.inode_size();
        let bg = self.read_block_group(bg_id)?;
        let id_in_bg = ((inode_id - 1) % inodes_per_group) as usize;

        let block_id =
            bg.desc.inode_table_first_block() + (id_in_bg * inode_size / BLOCK_SIZE) as PBlockId;
        let offset = (id_in_bg * inode_size) % BLOCK_SIZE;
        Ok((block_id, offset))
    }

    /// Get disk position of a block group. Return block id and offset within the block.
    #[allow(unused)]
    fn block_group_disk_pos(&self, block_group_id: BlockGroupId) -> Result<(PBlockId, usize)> {
        let super_block = self.read_super_block_cached();
        if block_group_id >= super_block.block_group_count() {
            return Err(crate::format_error!(
                ErrCode::EINVAL,
                "Invalid block group id {}",
                block_group_id
            ));
        }
        let desc_per_block = BLOCK_SIZE as u32 / super_block.desc_size() as u32;

        let block_id = super_block.first_data_block() + block_group_id / desc_per_block + 1;
        let offset = (block_group_id % desc_per_block) * super_block.desc_size() as u32;
        Ok((block_id as PBlockId, offset as usize))
    }
}
