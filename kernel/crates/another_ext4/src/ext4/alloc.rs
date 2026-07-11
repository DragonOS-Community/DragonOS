use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use crate::return_error;

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
        bgid as PBlockId * sb.blocks_per_group() as PBlockId
    }

    fn block_group_block_count(sb: &SuperBlock, bgid: BlockGroupId) -> usize {
        let first = Self::block_group_first_block(sb, bgid);
        let total = sb.block_count();
        if first >= total {
            return 0;
        }
        core::cmp::min(sb.blocks_per_group() as u64, total - first) as usize
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
            // Deallocate the block
            self.dealloc_block(inode, pblock)?;
            // Clear the block content
            self.write_block(&Block::new(pblock, Box::new([0; BLOCK_SIZE])))?;
        }
        // Free extent tree
        let pblocks = self.extent_all_tree_blocks(inode)?;
        for pblock in pblocks {
            // Deallocate the block
            self.dealloc_block(inode, pblock)?;
            // Clear the block content
            self.write_block(&Block::new(pblock, Box::new([0; BLOCK_SIZE])))?;
        }
        // Free xattr block
        let xattr_block = inode.inode.xattr_block();
        if xattr_block != 0 {
            // Deallocate the block
            self.dealloc_block(inode, xattr_block)?;
            // Clear the block content
            self.write_block(&Block::new(xattr_block, Box::new([0; BLOCK_SIZE])))?;
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
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(handle.inode_id)].lock();
        if !self.inode_is_allocated(handle.inode_id)? {
            return_error!(
                ErrCode::EINVAL,
                "Reclaim capability references free inode {}",
                handle.inode_id
            );
        }
        let mut inode = self.read_inode_uncached(handle.inode_id)?;
        if inode.inode.mode().bits() == 0
            || inode.inode.link_count() != 0
            || inode.inode.generation() != handle.generation
        {
            return_error!(
                ErrCode::EINVAL,
                "Invalid or stale reclaim capability for inode {}",
                handle.inode_id
            );
        }
        if let Err(error) = self.free_inode(&mut inode) {
            self.poison(ErrCode::EIO);
            return Err(error);
        }
        Ok(())
    }

    fn inode_is_allocated(&self, inode_id: InodeId) -> Result<bool> {
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
        // Update block count: data blocks only (tree blocks are added by setattr)
        inode.inode.set_fs_block_count(iblock as u64 + 1);
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
            let mut bitmap = Bitmap::new(&mut *bitmap_block.data, blocks_in_group);

            let bit = match bitmap.find_and_set_first_clear_bit(0, blocks_in_group) {
                Some(bit) => bit,
                None => continue,
            };
            let fblock = Self::block_group_first_block(&sb, bgid) + bit as PBlockId;

            // Set block group checksum
            bg.desc.set_block_bitmap_csum(&sb.uuid(), &bitmap);
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

    /// Deallocate a physical block allocated for an inode
    pub(super) fn dealloc_block(&self, _inode: &mut InodeRef, pblock: PBlockId) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();
        if pblock >= sb.block_count() {
            return_error!(ErrCode::EINVAL, "Invalid block {}", pblock);
        }

        let bgid = (pblock / sb.blocks_per_group() as PBlockId) as BlockGroupId;
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
        let mut bitmap = Bitmap::new(&mut *bitmap_block.data, blocks_in_group);

        // Free the block
        if bitmap.is_bit_clear(bit) {
            return_error!(ErrCode::EINVAL, "Block {} is already free", pblock);
        }
        bitmap.clear_bit(bit);
        // Set block group checksum
        bg.desc.set_block_bitmap_csum(&sb.uuid(), &bitmap);
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
            let mut bitmap = Bitmap::new(&mut *bitmap_block.data, inode_count);

            // Find a free inode
            let idx_in_bg =
                bitmap
                    .find_and_set_first_clear_bit(0, inode_count)
                    .ok_or(format_error!(
                        ErrCode::ENOSPC,
                        "No free inodes in block group {}",
                        bgid
                    ))? as u32;
            // Update bitmap in disk
            bg.desc.set_inode_bitmap_csum(&sb.uuid(), &bitmap);
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
        // Update bitmap in disk
        bg.desc.set_inode_bitmap_csum(&sb.uuid(), &bitmap);
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
