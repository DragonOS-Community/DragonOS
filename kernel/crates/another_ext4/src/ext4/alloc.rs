use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use crate::return_error;

fn extent_tail_batch_limit(
    first_data_block: PBlockId,
    blocks_per_group: PBlockId,
    start: PBlockId,
    count: u32,
) -> Option<u32> {
    if blocks_per_group == 0 || count == 0 || start < first_data_block {
        return None;
    }
    let last = start.checked_add(count as PBlockId)?.checked_sub(1)?;
    let in_last_group = (last - first_data_block) % blocks_per_group + 1;
    Some(core::cmp::min(count, in_last_group as u32))
}

fn linked_orphan_tail_remove_limit(
    keep_blocks: u64,
    tail_start: u32,
    tail_blocks: u32,
    group_limit: u32,
) -> Option<u32> {
    let tail_end = tail_start as u64 + tail_blocks as u64;
    if tail_end <= keep_blocks {
        return None;
    }
    let beyond_eof = tail_end - core::cmp::max(keep_blocks, tail_start as u64);
    Some(core::cmp::min(
        group_limit,
        core::cmp::min(beyond_eof, u32::MAX as u64) as u32,
    ))
}

impl Ext4 {
    fn restore_inode_allocation_state(
        &self,
        bitmap_block: &Block,
        bg: &BlockGroupRef,
        sb: &SuperBlock,
    ) -> Result<()> {
        self.write_block(bitmap_block)?;
        self.write_block_group_with_csum(&mut BlockGroupRef::new(bg.id, bg.desc))?;
        self.write_super_block(sb)
    }
    fn restore_block_allocation_state(
        &self,
        bitmap_block: &Block,
        bg: &BlockGroupRef,
        sb: &SuperBlock,
    ) -> Result<()> {
        self.write_block(bitmap_block)?;
        self.write_block_group_with_csum(&mut BlockGroupRef::new(bg.id, bg.desc))?;
        self.write_super_block(sb)
    }

    fn block_group_first_block(sb: &SuperBlock, bgid: BlockGroupId) -> PBlockId {
        sb.first_data_block() as PBlockId + bgid as PBlockId * sb.blocks_per_group() as PBlockId
    }

    fn block_group_block_count(sb: &SuperBlock, bgid: BlockGroupId) -> usize {
        let first = Self::block_group_first_block(sb, bgid);
        let total = sb.block_count();
        if first >= total {
            return 0;
        }
        core::cmp::min(sb.blocks_per_group() as u64, total - first) as usize
    }

    /// Stage the release of one contiguous physical-block range.
    ///
    /// This changes allocation metadata only.  In particular, freed data is
    /// not zeroed: after commit the blocks no longer belong to this inode and
    /// clearing them would be both unnecessary I/O and a race with reuse.
    pub(super) fn transaction_dealloc_block_range(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        first: PBlockId,
        count: u32,
    ) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        if count == 0 {
            return_error!(ErrCode::EINVAL, "Cannot free an empty block range");
        }

        let mut sb = self.transaction_read_super_block(transaction)?;
        let last_exclusive = first
            .checked_add(count as PBlockId)
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Block range overflows"))?;
        if first < sb.first_data_block() as PBlockId
            || first >= sb.block_count()
            || last_exclusive > sb.block_count()
        {
            return_error!(
                ErrCode::EINVAL,
                "Invalid block range {}..{}",
                first,
                last_exclusive
            );
        }
        let blocks_per_group = sb.blocks_per_group() as PBlockId;
        let data_first = sb.first_data_block() as PBlockId;
        let bgid = ((first - data_first) / blocks_per_group) as BlockGroupId;
        if ((last_exclusive - 1 - data_first) / blocks_per_group) as BlockGroupId != bgid {
            return_error!(ErrCode::EINVAL, "Block range crosses a block group");
        }
        let group_first = Self::block_group_first_block(&sb, bgid);
        let bit = (first - group_first) as usize;
        let count = count as usize;
        let blocks_in_group = Self::block_group_block_count(&sb, bgid);
        if bit
            .checked_add(count)
            .is_none_or(|end| end > blocks_in_group)
        {
            return_error!(ErrCode::EINVAL, "Block range exceeds block group");
        }
        self.validate_data_blocks(first, count as u64)?;

        let mut bg = self.transaction_read_block_group(transaction, bgid)?;
        let bitmap_block_id = bg.desc.block_bitmap_block();
        let metadata_csum =
            sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM);
        let checksum_bytes = (sb.clusters_per_group() as usize) / 8;
        if metadata_csum {
            if !bg.verify_checksum(&sb.uuid()) {
                return_error!(ErrCode::EIO, "Corrupt block-group descriptor checksum");
            }
            let bitmap_image = transaction.read(self.block_device.as_ref(), bitmap_block_id)?;
            if !bg
                .desc
                .verify_block_bitmap_csum(&sb.uuid(), &*bitmap_image, checksum_bytes)
            {
                return_error!(ErrCode::EIO, "Corrupt block bitmap checksum");
            }
        }
        // Validate all accounting before mutating the transaction image.  A
        // corrupt counter must not leave a half-applied bitmap update behind.
        let new_bg_free = bg
            .desc
            .get_free_blocks_count()
            .checked_add(count as u64)
            .filter(|value| *value <= blocks_in_group as u64)
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid block-group free count"))?;
        let new_sb_free = sb
            .free_blocks_count()
            .checked_add(count as u64)
            .filter(|value| *value <= sb.block_count())
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid filesystem free count"))?;
        {
            let image = self.transaction_block_for_update(transaction, bitmap_block_id)?;
            let mut bitmap = Bitmap::new(image, blocks_in_group);
            if (bit..bit + count).any(|index| bitmap.is_bit_clear(index)) {
                return_error!(ErrCode::EINVAL, "Block range contains a free block");
            }
            for index in bit..bit + count {
                bitmap.clear_bit(index);
            }
            if metadata_csum
                && !bg
                    .desc
                    .update_block_bitmap_csum(&sb.uuid(), image, checksum_bytes)
            {
                return_error!(ErrCode::EIO, "Invalid block bitmap checksum length");
            }
        }

        bg.desc.set_free_blocks_count(new_bg_free);
        self.transaction_stage_block_group_with_csum(transaction, &mut bg, &sb.uuid())?;
        sb.set_free_blocks_count(new_sb_free);
        self.transaction_stage_super_block(transaction, &sb)
    }

    /// Stage final inode-number release.  `itable_unused` describes the
    /// never-initialized tail of the inode table, not reusable inode slots, so
    /// freeing an inode must leave it unchanged (as Linux ext4 does).
    pub(super) fn transaction_dealloc_inode(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode_id: InodeId,
        is_dir: bool,
    ) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.transaction_read_super_block(transaction)?;
        if inode_id == 0 || inode_id > sb.inode_count() {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", inode_id);
        }
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode_id - 1) / inodes_per_group) as BlockGroupId;
        let idx = ((inode_id - 1) % inodes_per_group) as usize;
        let inode_count = sb.inode_count_in_group(bgid) as usize;
        if idx >= inode_count {
            return_error!(ErrCode::EINVAL, "Invalid inode index {}", idx);
        }

        let mut bg = self.transaction_read_block_group(transaction, bgid)?;
        let bitmap_block_id = bg.desc.inode_bitmap_block();
        let metadata_csum =
            sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM);
        let checksum_bytes = (sb.inodes_per_group() as usize) / 8;
        if metadata_csum {
            if !bg.verify_checksum(&sb.uuid()) {
                return_error!(ErrCode::EIO, "Corrupt block-group descriptor checksum");
            }
            let bitmap_image = transaction.read(self.block_device.as_ref(), bitmap_block_id)?;
            if !bg
                .desc
                .verify_inode_bitmap_csum(&sb.uuid(), &*bitmap_image, checksum_bytes)
            {
                return_error!(ErrCode::EIO, "Corrupt inode bitmap checksum");
            }
        }
        let new_bg_free = bg
            .desc
            .free_inodes_count()
            .checked_add(1)
            .filter(|value| *value <= inode_count as u32)
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid block-group inode count"))?;
        let new_sb_free = sb
            .free_inodes_count()
            .checked_add(1)
            .filter(|value| *value <= sb.inode_count())
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid filesystem inode count"))?;
        let new_used_dirs =
            if is_dir {
                Some(bg.desc.used_dirs_count().checked_sub(1).ok_or_else(|| {
                    format_error!(ErrCode::EINVAL, "Invalid used-directory count")
                })?)
            } else {
                None
            };
        {
            let image = self.transaction_block_for_update(transaction, bitmap_block_id)?;
            let mut bitmap = Bitmap::new(image, inode_count);
            if bitmap.is_bit_clear(idx) {
                return_error!(ErrCode::EINVAL, "Inode {} is already free", inode_id);
            }
            bitmap.clear_bit(idx);
            if metadata_csum
                && !bg
                    .desc
                    .update_inode_bitmap_csum(&sb.uuid(), image, checksum_bytes)
            {
                return_error!(ErrCode::EIO, "Invalid inode bitmap checksum length");
            }
        }

        bg.desc.set_free_inodes_count(new_bg_free);
        if let Some(used) = new_used_dirs {
            bg.desc.set_used_dirs_count(used);
        }
        self.transaction_stage_block_group_with_csum(transaction, &mut bg, &sb.uuid())?;
        sb.set_free_inodes_count(new_sb_free);
        self.transaction_stage_super_block(transaction, &sb)
    }

    /// Create a new inode, returning the inode and its number
    #[inline(never)]
    pub(super) fn create_inode(&self, mode: InodeMode) -> Result<InodeRef> {
        self.ensure_mutable()?;
        // Allocate an inode
        let is_dir = mode.file_type() == FileType::Directory;
        let id = self.alloc_inode(is_dir)?;

        let initialized = (|| {
            let generation = self.next_inode_generation(id)?;
            let mut inode = Box::new(Inode::default());
            inode.set_generation(generation);
            inode.set_mode(mode);
            inode.extent_init();
            let mut inode_ref = InodeRef::new(id, inode);
            self.write_inode_with_csum(&mut inode_ref)?;
            Ok(inode_ref)
        })();
        let inode_ref = match initialized {
            Ok(inode_ref) => inode_ref,
            Err(error) => {
                if self.rollback_new_inode(id, is_dir).is_err() {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }
        };

        trace!("Alloc inode {} ok", inode_ref.id);
        Ok(inode_ref)
    }

    /// Create a device inode (character or block device).
    ///
    /// Unlike `create_inode()`, this function:
    /// - Does NOT initialize the extent tree
    /// - Stores the device number in i_block[0..1] (Linux ext4 standard)
    #[inline(never)]
    pub(super) fn create_device_inode(
        &self,
        mode: InodeMode,
        major: u32,
        minor: u32,
    ) -> Result<InodeRef> {
        self.ensure_mutable()?;
        // Device nodes are never directories
        let id = self.alloc_inode(false)?;

        let initialized = (|| {
            let generation = self.next_inode_generation(id)?;
            let mut inode = Box::new(Inode::default());
            inode.set_generation(generation);
            inode.set_mode(mode);
            inode.set_device(major, minor);
            let mut inode_ref = InodeRef::new(id, inode);
            self.write_inode_with_csum(&mut inode_ref)?;
            Ok(inode_ref)
        })();
        let inode_ref = match initialized {
            Ok(inode_ref) => inode_ref,
            Err(error) => {
                if self.rollback_new_inode(id, false).is_err() {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }
        };

        trace!(
            "Alloc device inode {} ({}:{}) ok",
            inode_ref.id,
            major,
            minor
        );
        Ok(inode_ref)
    }

    /// Create(initialize) the root inode of the file system
    #[inline(never)]
    pub(super) fn create_root_inode(&self) -> Result<InodeRef> {
        let mut inode = Box::new(Inode::default());
        inode.set_mode(InodeMode::from_type_and_perm(
            FileType::Directory,
            InodeMode::from_bits_retain(0o755),
        ));
        inode.extent_init();

        let mut root = InodeRef::new(EXT4_ROOT_INO, inode);
        let root_self = root.clone();

        // Add `.` and `..` entries
        self.dir_add_entry(&mut root, &root_self, ".")?;
        self.dir_add_entry(&mut root, &root_self, "..")?;
        root.inode.set_link_count(2);

        self.write_inode_with_csum(&mut root)?;
        Ok(root)
    }

    /// Free an allocated inode and all data blocks allocated for it
    pub(super) fn free_inode(&self, inode: &mut InodeRef) -> Result<()> {
        let inode_id = inode.id;
        // Free the data blocks allocated for the inode
        let pblocks = self.extent_all_data_blocks(inode)?;
        for pblock in pblocks {
            self.dealloc_block(inode, pblock)?;
        }
        // Free extent tree
        let pblocks = self.extent_all_tree_blocks(inode)?;
        for pblock in pblocks {
            self.dealloc_block(inode, pblock)?;
        }
        // Free xattr block
        let xattr_block = inode.inode.xattr_block();
        if xattr_block != 0 {
            self.dealloc_block(inode, xattr_block)?;
        }
        // Deallocate the inode
        self.dealloc_inode(inode)?;
        // Invalidate inode cache entry
        self.inode_cache.lock().invalidate(inode_id);
        Ok(())
    }

    fn next_inode_generation(&self, inode_id: InodeId) -> Result<u32> {
        let previous = self.read_inode_uncached(inode_id)?.inode.generation();
        let next = previous.wrapping_add(1);
        Ok(if next == 0 { 1 } else { next })
    }

    fn rollback_new_inode(&self, inode_id: InodeId, is_dir: bool) -> Result<()> {
        // The inode-table slot is the authoritative lifetime identity. It may
        // still contain the previous generation, or the newly initialized
        // generation if the write completed before reporting an error.
        let generation = self.read_inode_uncached(inode_id)?.inode.generation();
        let mut inode = Box::new(Inode::default());
        inode.set_generation(generation);
        inode.set_mode(if is_dir {
            InodeMode::DIRECTORY
        } else {
            InodeMode::FILE
        });
        self.dealloc_inode(&mut InodeRef::new(inode_id, inode))
    }

    /// Physically reclaim the inode lifetime represented by `handle`.
    ///
    /// Validation and reclamation share the inode mutation shard.  The inode is
    /// re-read from disk so an unlink-time value snapshot can never discard
    /// blocks or xattrs added by later writeback.
    pub fn reclaim_inode(
        &self,
        handle: InodeReclaimHandle,
    ) -> core::result::Result<(), InodeReclaimError> {
        match self.reclaim_inode_inner(&handle) {
            Ok(()) => Ok(()),
            Err(error) => Err(InodeReclaimError::new(error, handle)),
        }
    }

    fn reclaim_inode_inner(&self, handle: &InodeReclaimHandle) -> Result<()> {
        self.ensure_mutable()?;
        // Delayed VFS eviction runs after the namespace operation released its
        // guard. Re-enter the transactional domain for the complete multi-
        // transaction reclaim so no legacy direct writer can race snapshots.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(handle.inode_id)].lock();
        self.reclaim_inode_lifetime(handle.inode_id, handle.generation)
    }

    /// Reclaim a mount-recovery orphan without manufacturing a VFS lifetime
    /// capability. The generation is read authoritatively before entering the
    /// same validated orchestration used by delayed final close.
    pub(super) fn reclaim_orphan_inode_by_id(&self, inode_id: InodeId) -> Result<()> {
        self.ensure_mutable()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode_id)].lock();
        let generation = self.read_inode_uncached(inode_id)?.inode.generation();
        self.reclaim_inode_lifetime(inode_id, generation)
    }

    /// Complete a crash-interrupted truncate for an inode that still has names.
    /// Blocks at or beyond ceil(i_size / block_size) are removed in restartable
    /// transactions; the inode itself and its xattrs remain live.
    pub(super) fn recover_linked_orphan_inode_by_id(&self, inode_id: InodeId) -> Result<()> {
        self.ensure_mutable()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode_id)].lock();
        if !self.legacy_orphan_contains(inode_id)? {
            return_error!(ErrCode::EINVAL, "Inode {} is not orphaned", inode_id);
        }
        loop {
            let mut inode = self.read_inode_uncached(inode_id)?;
            let sb = self.read_super_block_cached();
            if !self.inode_is_allocated(inode_id)?
                || inode.inode.mode().bits() == 0
                || inode.inode.link_count() == 0
                || !super::orphan::inode_checksum_valid(&sb, &inode)
                || !inode.inode.is_file()
                || !inode.inode.uses_extents()
            {
                return_error!(ErrCode::EIO, "Invalid linked truncate orphan {}", inode_id);
            }
            let keep_blocks = inode.inode.size().div_ceil(BLOCK_SIZE as u64);
            let mut transaction = self.transaction_start(32)?;
            let Some(tail) = self.extent_tail(&transaction, &inode)? else {
                transaction.abort();
                break;
            };
            let extent_end = tail
                .start_pblock
                .checked_add(tail.block_count as PBlockId)
                .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent physical range"))?;
            if tail.start_pblock == 0
                || extent_end > sb.block_count()
                || self.journal_owns_block_range(tail.start_pblock, extent_end)
            {
                return_error!(ErrCode::EIO, "Invalid linked orphan extent");
            }
            let group_limit = extent_tail_batch_limit(
                sb.first_data_block() as PBlockId,
                sb.blocks_per_group() as PBlockId,
                tail.start_pblock,
                tail.block_count,
            )
            .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent tail"))?;
            let Some(remove_limit) = linked_orphan_tail_remove_limit(
                keep_blocks,
                tail.start_lblock,
                tail.block_count,
                group_limit,
            ) else {
                transaction.abort();
                break;
            };
            let removed = self
                .extent_remove_tail_in_transaction(&mut transaction, &mut inode, remove_limit)?
                .ok_or_else(|| format_error!(ErrCode::EIO, "Extent tail disappeared"))?;
            self.transaction_dealloc_block_range(
                &mut transaction,
                removed.start_pblock,
                removed.block_count,
            )?;
            for metadata in removed.metadata_blocks.iter().copied() {
                self.transaction_dealloc_block_range(&mut transaction, metadata, 1)?;
            }
            let released = removed.block_count as u64 + removed.metadata_blocks.len() as u64;
            inode.inode.set_fs_block_count(
                inode
                    .inode
                    .fs_block_count()
                    .checked_sub(released)
                    .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid inode block count"))?,
            );
            self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
            self.commit_reclaim_transaction(transaction)?;
        }

        let mut inode = self.read_inode_uncached(inode_id)?;
        if inode.inode.link_count() == 0 {
            return_error!(ErrCode::EIO, "Linked truncate orphan lost all links");
        }
        let mut transaction = self.transaction_start(8)?;
        let mut sb = self.transaction_read_super_block(&transaction)?;
        self.transaction_orphan_del(&mut transaction, &inode, &mut sb)?;
        inode.inode.set_next_orphan(0);
        self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
        self.commit_reclaim_transaction(transaction)
    }

    fn reclaim_inode_lifetime(&self, inode_id: InodeId, generation: u32) -> Result<()> {
        self.validate_reclaim_inode(inode_id, generation)?;
        if !self.legacy_orphan_contains(inode_id)? {
            return_error!(ErrCode::EINVAL, "Inode {} is not orphaned", inode_id);
        }
        // Each iteration starts from the checkpointed inode-table entry.  The
        // on-disk extent root is therefore the restart cursor after any crash.
        // Chain membership was fully validated once above. The metadata write
        // barrier keeps the chain stable, avoiding O(extents * orphan_count)
        // repeated walks; final orphan_del performs its own bounded walk.
        loop {
            let mut inode = self.validate_reclaim_inode(inode_id, generation)?;
            if !inode.inode.uses_extents() {
                if inode.inode.fs_block_count() != 0 {
                    return_error!(ErrCode::EIO, "Non-extent orphan owns blocks");
                }
                break;
            }

            let mut transaction = self.transaction_start(32)?;
            let Some(tail) = self.extent_tail(&transaction, &inode)? else {
                break;
            };
            let extent_end = tail
                .start_pblock
                .checked_add(tail.block_count as PBlockId)
                .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent physical range"))?;
            if tail.start_pblock == 0
                || extent_end > self.read_super_block_cached().block_count()
                || self.journal_owns_block_range(tail.start_pblock, extent_end)
            {
                return_error!(
                    ErrCode::EIO,
                    "Orphan extent overlaps invalid or journal-owned blocks"
                );
            }
            // transaction_dealloc_block_range deliberately accepts one block
            // group only. Trim the right edge at that boundary so bitmap and
            // counters stay a compact, independently restartable unit.
            let sb = self.read_super_block_cached();
            let blocks_per_group = sb.blocks_per_group() as PBlockId;
            let remove_limit = extent_tail_batch_limit(
                sb.first_data_block() as PBlockId,
                blocks_per_group,
                tail.start_pblock,
                tail.block_count,
            )
            .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent tail"))?;
            let removed = self
                .extent_remove_tail_in_transaction(&mut transaction, &mut inode, remove_limit)?
                .ok_or_else(|| format_error!(ErrCode::EIO, "Extent tail disappeared"))?;
            self.transaction_dealloc_block_range(
                &mut transaction,
                removed.start_pblock,
                removed.block_count,
            )?;
            for metadata in removed.metadata_blocks.iter().copied() {
                self.transaction_dealloc_block_range(&mut transaction, metadata, 1)?;
            }
            let released = removed.block_count as u64 + removed.metadata_blocks.len() as u64;
            let remaining = inode
                .inode
                .fs_block_count()
                .checked_sub(released)
                .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid inode block count"))?;
            inode.inode.set_fs_block_count(remaining);
            inode.inode.set_size(core::cmp::min(
                inode.inode.size(),
                removed.start_lblock as u64 * BLOCK_SIZE as u64,
            ));
            self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
            self.commit_reclaim_transaction(transaction)?;
        }

        // External xattrs form their own restartable transaction. Shared
        // blocks update h_refcount; exclusive blocks also release allocation.
        let mut inode = self.validate_reclaim_inode(inode_id, generation)?;
        if inode.inode.xattr_block() != 0 {
            let mut transaction = self.transaction_start(16)?;
            if let Some(block) = self.transaction_release_xattr(&mut transaction, &mut inode)? {
                self.transaction_dealloc_block_range(&mut transaction, block, 1)?;
            }
            self.commit_reclaim_transaction(transaction)?;
        }

        // Only the final transaction makes the orphan undiscoverable. It also
        // frees the inode number and installs a checksum-correct cleared table
        // entry, preserving generation so reuse advances lifetime identity.
        let inode = self.validate_reclaim_inode(inode_id, generation)?;
        if inode.inode.uses_extents() {
            let transaction = self.transaction_start(1)?;
            if self.extent_tail(&transaction, &inode)?.is_some() {
                return_error!(ErrCode::EIO, "Final reclaim with live extent");
            }
            transaction.abort();
        }
        if inode.inode.xattr_block() != 0 || inode.inode.fs_block_count() != 0 {
            return_error!(ErrCode::EIO, "Final reclaim with owned blocks");
        }
        let is_dir = inode.inode.is_dir();
        let mut transaction = self.transaction_start(16)?;
        let mut sb = self.transaction_read_super_block(&transaction)?;
        self.transaction_orphan_del(&mut transaction, &inode, &mut sb)?;
        self.transaction_dealloc_inode(&mut transaction, inode_id, is_dir)?;
        let mut cleared = InodeRef::new(inode_id, Box::default());
        cleared.inode.set_generation(generation);
        self.transaction_stage_inode_with_csum(&mut transaction, &mut cleared)?;
        self.commit_reclaim_transaction(transaction)
    }

    fn validate_reclaim_inode(&self, inode_id: InodeId, generation: u32) -> Result<InodeRef> {
        if !self.inode_is_allocated(inode_id)? {
            return_error!(
                ErrCode::EINVAL,
                "Reclaim references free inode {}",
                inode_id
            );
        }
        let inode = self.read_inode_uncached(inode_id)?;
        let sb = self.read_super_block_cached();
        if inode.inode.mode().bits() == 0
            || inode.inode.link_count() != 0
            || inode.inode.generation() != generation
            || !super::orphan::inode_checksum_valid(&sb, &inode)
        {
            return_error!(
                ErrCode::EINVAL,
                "Invalid or stale orphan inode {}",
                inode_id
            );
        }
        Ok(inode)
    }

    fn commit_reclaim_transaction(
        &self,
        transaction: super::journal_transaction::Transaction<'_>,
    ) -> Result<()> {
        if let Err(error) = transaction.commit(self.block_device.as_ref(), self) {
            self.poison(ErrCode::EIO);
            return Err(error.error);
        }
        Ok(())
    }

    pub(super) fn inode_is_allocated(&self, inode_id: InodeId) -> Result<bool> {
        let _alloc_guard = self.alloc_lock.lock();
        let sb = self.read_super_block_cached();
        if inode_id == 0 || inode_id > sb.inode_count() {
            return_error!(ErrCode::EINVAL, "Invalid inode number {}", inode_id);
        }
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode_id - 1) / inodes_per_group) as BlockGroupId;
        if bgid >= sb.block_group_count() {
            return_error!(ErrCode::EINVAL, "Invalid inode block group {}", bgid);
        }
        let idx_in_bg = (inode_id - 1) % inodes_per_group;
        let bg = self.read_block_group(bgid)?;
        let bitmap_block = self.read_block(bg.desc.inode_bitmap_block())?;
        let inode_count = sb.inode_count_in_group(bgid) as usize;
        if idx_in_bg as usize >= inode_count {
            return_error!(ErrCode::EINVAL, "Invalid inode index {}", idx_in_bg);
        }
        let mut bitmap_data = bitmap_block.data.clone();
        let bitmap = Bitmap::new(&mut *bitmap_data, inode_count);
        Ok(!bitmap.is_bit_clear(idx_in_bg as usize))
    }

    /// Append a data block for an inode, return a pair of (logical block id, physical block id)
    ///
    /// Only data blocks allocated by `inode_append_block` will be counted in `inode.block_count`.
    /// Blocks allocated by calling `alloc_block` directly will not be counted, i.e., blocks
    /// allocated for the inode's extent tree.
    ///
    /// Appending a block does not increase `inode.size`, because `inode.size` records the actual
    /// size of the data content, not the number of blocks allocated for it.
    ///
    /// If the inode is a file, `inode.size` will be increased when writing to end of the file.
    /// If the inode is a directory, `inode.size` will be increased when adding a new entry to the
    /// newly created block.
    pub(super) fn inode_append_block(&self, inode: &mut InodeRef) -> Result<(LBlockId, PBlockId)> {
        // Determine the next logical block from the extent tree.
        // We cannot use fs_block_count() because i_blocks may include tree
        // metadata blocks (added by setattr after the allocation loop).
        let iblock = self.extent_next_data_lblock(inode)?;
        // Check the extent tree to get the physical block id
        let fblock = self.extent_query_or_create(inode, iblock, 1)?;
        let total_blocks = self
            .extent_all_data_blocks(inode)?
            .len()
            .checked_add(self.extent_all_tree_blocks(inode)?.len())
            .ok_or_else(|| format_error!(ErrCode::EFBIG, "Inode blocks overflow"))?;
        inode.inode.set_fs_block_count(total_blocks as u64);
        self.write_inode_with_csum(inode)?;

        Ok((iblock, fblock))
    }

    /// Allocate a new physical block for an inode, return the physical block number
    pub(super) fn alloc_block(&self, inode: &mut InodeRef) -> Result<PBlockId> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();
        let inodes_per_group = sb.inodes_per_group();
        let preferred_bgid = ((inode.id - 1) / inodes_per_group) as BlockGroupId;
        let bg_count = sb.block_group_count();

        for i in 0..bg_count {
            let bgid = (preferred_bgid + i) % bg_count;
            let blocks_in_group = Self::block_group_block_count(&sb, bgid);
            if blocks_in_group == 0 {
                continue;
            }

            // Load block group descriptor
            let mut bg = self.read_block_group(bgid)?;
            if bg.desc.get_free_blocks_count() == 0 {
                continue;
            }

            // Load block bitmap. Bits are relative to the start of this block group;
            // extent physical block numbers are absolute filesystem block numbers.
            let bitmap_block_id = bg.desc.block_bitmap_block();
            let mut bitmap_block = self.read_block(bitmap_block_id)?;
            let old_bitmap_block = bitmap_block.clone();
            let old_bg = BlockGroupRef::new(bg.id, bg.desc);
            let old_sb = sb;
            let bit = {
                let mut bitmap = Bitmap::new(&mut *bitmap_block.data, blocks_in_group);
                match bitmap.find_and_set_first_clear_bit(0, blocks_in_group) {
                    Some(bit) => bit,
                    None => continue,
                }
            };
            let fblock = Self::block_group_first_block(&sb, bgid) + bit as PBlockId;

            // Set block group checksum
            if !bg.desc.update_block_bitmap_csum(
                &sb.uuid(),
                &*bitmap_block.data,
                sb.clusters_per_group() as usize / 8,
            ) {
                return_error!(ErrCode::EIO, "Invalid block bitmap checksum length");
            }
            self.write_block(&bitmap_block)?;

            // Update block group counters
            bg.desc
                .set_free_blocks_count(bg.desc.get_free_blocks_count() - 1);
            if let Err(err) = self.write_block_group_with_csum(&mut bg) {
                return match self.restore_block_allocation_state(
                    &old_bitmap_block,
                    &old_bg,
                    &old_sb,
                ) {
                    Ok(()) => Err(err),
                    Err(rollback_err) => Err(rollback_err),
                };
            }

            // Update superblock counters
            sb.set_free_blocks_count(sb.free_blocks_count() - 1);
            if let Err(err) = self.write_super_block(&sb) {
                return match self.restore_block_allocation_state(
                    &old_bitmap_block,
                    &old_bg,
                    &old_sb,
                ) {
                    Ok(()) => Err(err),
                    Err(rollback_err) => Err(rollback_err),
                };
            }

            trace!("Alloc block {} ok", fblock);
            return Ok(fblock);
        }

        return_error!(ErrCode::ENOSPC, "No free blocks in filesystem");
    }

    /// Allocate and initialize a data block before any extent can publish it.
    /// Extent-tree and xattr metadata allocations deliberately use
    /// `alloc_block()` directly because their callers construct metadata
    /// images rather than exposing zero-filled file data.
    pub(super) fn alloc_zeroed_data_block(&self, inode: &mut InodeRef) -> Result<PBlockId> {
        self.alloc_initialized_data_block(inode, Box::new([0; BLOCK_SIZE]))
    }

    pub(super) fn alloc_initialized_data_block(
        &self,
        inode: &mut InodeRef,
        image: Box<[u8; BLOCK_SIZE]>,
    ) -> Result<PBlockId> {
        let pblock = self.alloc_block(inode)?;
        if let Err(init_error) = self.write_block(&Block::new(pblock, image)) {
            if let Err(rollback_error) = self.dealloc_block(inode, pblock) {
                // The allocation bit may still be set and no extent owns the
                // block.  Fail-stop instead of permitting silent leakage or a
                // later stale-data mapping on this mount.
                self.poison(ErrCode::EIO);
                return Err(rollback_error);
            }
            return Err(init_error);
        }
        Ok(pblock)
    }

    /// Deallocate a physical block allocated for an inode
    pub(super) fn dealloc_block(&self, _inode: &mut InodeRef, pblock: PBlockId) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();
        if pblock >= sb.block_count() {
            return_error!(ErrCode::EINVAL, "Invalid block {}", pblock);
        }

        if pblock < sb.first_data_block() as PBlockId {
            return_error!(ErrCode::EINVAL, "Invalid block {}", pblock);
        }
        let bgid = ((pblock - sb.first_data_block() as PBlockId)
            / sb.blocks_per_group() as PBlockId) as BlockGroupId;
        let bit = (pblock - Self::block_group_first_block(&sb, bgid)) as usize;
        let blocks_in_group = Self::block_group_block_count(&sb, bgid);
        if bit >= blocks_in_group {
            return_error!(ErrCode::EINVAL, "Invalid block {}", pblock);
        }

        // Load block group descriptor
        let mut bg = self.read_block_group(bgid)?;

        // Load block bitmap
        let bitmap_block_id = bg.desc.block_bitmap_block();
        let mut bitmap_block = self.read_block(bitmap_block_id)?;
        let old_bitmap_block = bitmap_block.clone();
        let old_bg = BlockGroupRef::new(bg.id, bg.desc);
        let old_sb = sb;
        {
            let mut bitmap = Bitmap::new(&mut *bitmap_block.data, blocks_in_group);
            // Free the block
            if bitmap.is_bit_clear(bit) {
                return_error!(ErrCode::EINVAL, "Block {} is already free", pblock);
            }
            bitmap.clear_bit(bit);
        }
        // Set block group checksum
        if !bg.desc.update_block_bitmap_csum(
            &sb.uuid(),
            &*bitmap_block.data,
            sb.clusters_per_group() as usize / 8,
        ) {
            return_error!(ErrCode::EIO, "Invalid block bitmap checksum length");
        }
        self.write_block(&bitmap_block)?;

        // Update block group counters
        bg.desc
            .set_free_blocks_count(bg.desc.get_free_blocks_count() + 1);
        if let Err(err) = self.write_block_group_with_csum(&mut bg) {
            return match self.restore_block_allocation_state(&old_bitmap_block, &old_bg, &old_sb) {
                Ok(()) => Err(err),
                Err(rollback_err) => Err(rollback_err),
            };
        }

        // Update superblock counters
        sb.set_free_blocks_count(sb.free_blocks_count() + 1);
        if let Err(err) = self.write_super_block(&sb) {
            return match self.restore_block_allocation_state(&old_bitmap_block, &old_bg, &old_sb) {
                Ok(()) => Err(err),
                Err(rollback_err) => Err(rollback_err),
            };
        }

        trace!("Free block {} ok", pblock);
        Ok(())
    }

    /// Allocate a new inode, returning the inode number.
    fn alloc_inode(&self, is_dir: bool) -> Result<InodeId> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();
        let bg_count = sb.block_group_count();

        let mut bgid = 0;
        while bgid < bg_count {
            // Load block group descriptor
            let mut bg = self.read_block_group(bgid)?;
            // If there are no free inodes in this block group, try the next one
            if bg.desc.free_inodes_count() == 0 {
                bgid += 1;
                continue;
            }
            // Load inode bitmap
            let bitmap_block_id = bg.desc.inode_bitmap_block();
            let mut bitmap_block = self.read_block(bitmap_block_id)?;
            let old_bitmap_block = bitmap_block.clone();
            let old_bg = BlockGroupRef::new(bg.id, bg.desc);
            let old_sb = sb;
            let inode_count = sb.inode_count_in_group(bgid) as usize;
            // Find a free inode, limiting allocation to real inodes even though
            // the checksum covers the fixed inodes_per_group bitmap length.
            let idx_in_bg = {
                let mut bitmap = Bitmap::new(&mut *bitmap_block.data, inode_count);
                bitmap
                    .find_and_set_first_clear_bit(0, inode_count)
                    .ok_or(format_error!(
                        ErrCode::ENOSPC,
                        "No free inodes in block group {}",
                        bgid
                    ))? as u32
            };
            // Update bitmap in disk
            if !bg.desc.update_inode_bitmap_csum(
                &sb.uuid(),
                &*bitmap_block.data,
                sb.inodes_per_group() as usize / 8,
            ) {
                return_error!(ErrCode::EIO, "Invalid inode bitmap checksum length");
            }
            self.write_block(&bitmap_block)?;

            // Modify block group counters
            bg.desc
                .set_free_inodes_count(bg.desc.free_inodes_count() - 1);
            if is_dir {
                bg.desc.set_used_dirs_count(bg.desc.used_dirs_count() + 1);
            }
            let mut unused = bg.desc.itable_unused();
            let free = inode_count as u32 - unused;
            if idx_in_bg >= free {
                unused = inode_count as u32 - (idx_in_bg + 1);
                bg.desc.set_itable_unused(unused);
            }
            if let Err(error) = self.write_block_group_with_csum(&mut bg) {
                if self
                    .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                    .is_err()
                {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }

            // Update superblock counters
            sb.set_free_inodes_count(sb.free_inodes_count() - 1);
            if let Err(error) = self.write_super_block(&sb) {
                if self
                    .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                    .is_err()
                {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }

            // Compute the absolute i-node number
            let inodes_per_group = sb.inodes_per_group();
            let inode_id = bgid * inodes_per_group + (idx_in_bg + 1);
            return Ok(inode_id);
        }
        trace!("no free inode");
        return_error!(ErrCode::ENOSPC, "No free inodes in block group {}", bgid);
    }

    /// Free an inode
    fn dealloc_inode(&self, inode_ref: &mut InodeRef) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();

        // Calc block group id and index in block group
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode_ref.id - 1) / inodes_per_group) as BlockGroupId;
        let idx_in_bg = (inode_ref.id - 1) % inodes_per_group;
        // Load block group descriptor
        let mut bg = self.read_block_group(bgid)?;
        // Load inode bitmap
        let bitmap_block_id = bg.desc.inode_bitmap_block();
        let mut bitmap_block = self.read_block(bitmap_block_id)?;
        let old_bitmap_block = bitmap_block.clone();
        let old_bg = BlockGroupRef::new(bg.id, bg.desc);
        let old_sb = sb;
        let inode_count = sb.inode_count_in_group(bgid) as usize;
        {
            let mut bitmap = Bitmap::new(&mut *bitmap_block.data, inode_count);
            // Free the inode
            if bitmap.is_bit_clear(idx_in_bg as usize) {
                return_error!(
                    ErrCode::EINVAL,
                    "Inode {} is already free in block group {}",
                    inode_ref.id,
                    bgid
                );
            }
            bitmap.clear_bit(idx_in_bg as usize);
        }
        // Update bitmap in disk
        if !bg.desc.update_inode_bitmap_csum(
            &sb.uuid(),
            &*bitmap_block.data,
            sb.inodes_per_group() as usize / 8,
        ) {
            return_error!(ErrCode::EIO, "Invalid inode bitmap checksum length");
        }
        self.write_block(&bitmap_block)?;

        // Update block group counters
        bg.desc
            .set_free_inodes_count(bg.desc.free_inodes_count() + 1);
        if inode_ref.inode.is_dir() {
            bg.desc.set_used_dirs_count(bg.desc.used_dirs_count() - 1);
        }
        bg.desc.set_itable_unused(bg.desc.itable_unused() + 1);
        if let Err(error) = self.write_block_group_with_csum(&mut bg) {
            if self
                .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                .is_err()
            {
                self.poison(ErrCode::EIO);
            }
            return Err(error);
        }

        // Update superblock counters
        sb.set_free_inodes_count(sb.free_inodes_count() + 1);
        if let Err(error) = self.write_super_block(&sb) {
            if self
                .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                .is_err()
            {
                self.poison(ErrCode::EIO);
            }
            return Err(error);
        }

        // Clear inode content while preserving the lifetime generation.  The
        // next allocation advances it before publishing the reused inode.
        let generation = inode_ref.inode.generation();
        *inode_ref.inode = Inode::default();
        inode_ref.inode.set_generation(generation);
        self.write_inode_with_csum(inode_ref)?;

        Ok(())
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::{extent_tail_batch_limit, linked_orphan_tail_remove_limit};

    #[test]
    fn extent_reclaim_batch_never_crosses_a_block_group() {
        // 1 KiB ext4 starts data at block 1. The tail spans groups 0 and 1;
        // only the two right-most blocks in group 1 may be removed together.
        assert_eq!(extent_tail_batch_limit(1, 8, 7, 4), Some(2));
        assert_eq!(extent_tail_batch_limit(0, 8, 9, 3), Some(3));
    }

    #[test]
    fn extent_reclaim_batch_rejects_invalid_or_overflowing_tails() {
        assert_eq!(extent_tail_batch_limit(1, 8, 0, 1), None);
        assert_eq!(extent_tail_batch_limit(0, 8, u64::MAX, 2), None);
        assert_eq!(extent_tail_batch_limit(0, 0, 1, 1), None);
    }

    #[test]
    fn linked_orphan_trim_preserves_eof_block_and_honors_group_batch() {
        assert_eq!(linked_orphan_tail_remove_limit(5, 3, 6, 6), Some(4));
        assert_eq!(linked_orphan_tail_remove_limit(5, 3, 6, 2), Some(2));
        assert_eq!(linked_orphan_tail_remove_limit(5, 8, 3, 3), Some(3));
        assert_eq!(linked_orphan_tail_remove_limit(12, 8, 3, 3), None);
    }
}
