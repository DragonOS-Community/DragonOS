use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use crate::return_error;

impl Ext4 {
    /// Create a new inode, returning the inode and its number
    #[inline(never)]
    pub(super) fn create_inode(&self, mode: InodeMode) -> Result<InodeRef> {
        // Allocate an inode
        let is_dir = mode.file_type() == FileType::Directory;
        let id = self.alloc_inode(is_dir)?;

        // Initialize the inode
        let mut inode = Box::new(Inode::default());
        inode.set_mode(mode);
        inode.extent_init();
        let mut inode_ref = InodeRef::new(id, inode);

        // Sync the inode to disk
        self.write_inode_with_csum(&mut inode_ref);

        trace!("Alloc inode {} ok", inode_ref.id);
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

        self.write_inode_with_csum(&mut root);
        Ok(root)
    }

    /// Free an allocated inode and all data blocks allocated for it
    pub(super) fn free_inode(&self, inode: &mut InodeRef) -> Result<()> {
        // Free the data blocks allocated for the inode
        let pblocks = self.extent_all_data_blocks(inode);
        for pblock in pblocks {
            // Deallocate the block
            self.dealloc_block(inode, pblock)?;
            // Clear the block content
            self.write_block(&Block::new(pblock, Box::new([0; BLOCK_SIZE])));
        }
        // Free extent tree
        let pblocks = self.extent_all_tree_blocks(inode);
        for pblock in pblocks {
            // Deallocate the block
            self.dealloc_block(inode, pblock)?;
            // Clear the block content
            self.write_block(&Block::new(pblock, Box::new([0; BLOCK_SIZE])));
        }
        // Free xattr block
        let xattr_block = inode.inode.xattr_block();
        if xattr_block != 0 {
            // Deallocate the block
            self.dealloc_block(inode, xattr_block)?;
            // Clear the block content
            self.write_block(&Block::new(xattr_block, Box::new([0; BLOCK_SIZE])));
        }
        // Deallocate the inode
        self.dealloc_inode(inode)?;
        Ok(())
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
        // The new logical block id
        let iblock = inode.inode.fs_block_count() as LBlockId;
        // Check the extent tree to get the physical block id
        let fblock = self.extent_query_or_create(inode, iblock, 1)?;
        // Update block count
        inode.inode.set_fs_block_count(iblock as u64 + 1);
        self.write_inode_with_csum(inode);

        Ok((iblock, fblock))
    }

    /// Allocate a new physical block for an inode, return the physical block number
    pub(super) fn alloc_block(&self, inode: &mut InodeRef) -> Result<PBlockId> {
        let mut sb = self.read_super_block();

        // Calc block group id
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode.id - 1) / inodes_per_group) as BlockGroupId;

        // Load block group descriptor
        let mut bg = self.read_block_group(bgid);

        // Load block bitmap
        let bitmap_block_id = bg.desc.block_bitmap_block();
        let mut bitmap_block = self.read_block(bitmap_block_id);
        let mut bitmap = Bitmap::new(&mut *bitmap_block.data, 8 * BLOCK_SIZE);

        // Find the first free block
        let fblock = bitmap
            .find_and_set_first_clear_bit(0, 8 * BLOCK_SIZE)
            .ok_or(format_error!(
                ErrCode::ENOSPC,
                "No free blocks in block group {}",
                bgid
            ))? as PBlockId;
        // Set block group checksum
        bg.desc.set_block_bitmap_csum(&sb.uuid(), &bitmap);
        self.write_block(&bitmap_block);

        // Update block group counters
        bg.desc
            .set_free_blocks_count(bg.desc.get_free_blocks_count() - 1);
        self.write_block_group_with_csum(&mut bg);

        // Update superblock counters
        sb.set_free_blocks_count(sb.free_blocks_count() - 1);
        self.write_super_block(&sb);

        trace!("Alloc block {} ok", fblock);
        Ok(fblock)
    }

    /// Deallocate a physical block allocated for an inode
    pub(super) fn dealloc_block(&self, inode: &mut InodeRef, pblock: PBlockId) -> Result<()> {
        let mut sb = self.read_super_block();

        // Calc block group id
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode.id - 1) / inodes_per_group) as BlockGroupId;

        // Load block group descriptor
        let mut bg = self.read_block_group(bgid);

        // Load block bitmap
        let bitmap_block_id = bg.desc.block_bitmap_block();
        let mut bitmap_block = self.read_block(bitmap_block_id);
        let mut bitmap = Bitmap::new(&mut *bitmap_block.data, 8 * BLOCK_SIZE);

        // Free the block
        if bitmap.is_bit_clear(pblock as usize) {
            return_error!(ErrCode::EINVAL, "Block {} is already free", pblock);
        }
        bitmap.clear_bit(pblock as usize);
        // Set block group checksum
        bg.desc.set_block_bitmap_csum(&sb.uuid(), &bitmap);
        self.write_block(&bitmap_block);

        // Update block group counters
        bg.desc
            .set_free_blocks_count(bg.desc.get_free_blocks_count() + 1);
        self.write_block_group_with_csum(&mut bg);

        // Update superblock counters
        sb.set_free_blocks_count(sb.free_blocks_count() + 1);
        self.write_super_block(&sb);

        trace!("Free block {} ok", pblock);
        Ok(())
    }

    /// Allocate a new inode, returning the inode number.
    fn alloc_inode(&self, is_dir: bool) -> Result<InodeId> {
        let mut sb = self.read_super_block();
        let bg_count = sb.block_group_count();

        let mut bgid = 0;
        while bgid <= bg_count {
            // Load block group descriptor
            let mut bg = self.read_block_group(bgid);
            // If there are no free inodes in this block group, try the next one
            if bg.desc.free_inodes_count() == 0 {
                bgid += 1;
                continue;
            }
            // Load inode bitmap
            let bitmap_block_id = bg.desc.inode_bitmap_block();
            let mut bitmap_block = self.read_block(bitmap_block_id);
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
            self.write_block(&bitmap_block);

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
            self.write_block_group_with_csum(&mut bg);

            // Update superblock counters
            sb.set_free_inodes_count(sb.free_inodes_count() - 1);
            self.write_super_block(&sb);

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
        let mut sb = self.read_super_block();

        // Calc block group id and index in block group
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode_ref.id - 1) / inodes_per_group) as BlockGroupId;
        let idx_in_bg = (inode_ref.id - 1) % inodes_per_group;
        // Load block group descriptor
        let mut bg = self.read_block_group(bgid);
        // Load inode bitmap
        let bitmap_block_id = bg.desc.inode_bitmap_block();
        let mut bitmap_block = self.read_block(bitmap_block_id);
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
        self.write_block(&bitmap_block);

        // Update block group counters
        bg.desc
            .set_free_inodes_count(bg.desc.free_inodes_count() + 1);
        if inode_ref.inode.is_dir() {
            bg.desc.set_used_dirs_count(bg.desc.used_dirs_count() - 1);
        }
        bg.desc.set_itable_unused(bg.desc.itable_unused() + 1);
        self.write_block_group_with_csum(&mut bg);

        // Update superblock counters
        sb.set_free_inodes_count(sb.free_inodes_count() + 1);
        self.write_super_block(&sb);

        // Clear inode content
        inode_ref.inode = Box::new(Inode::default());
        self.write_inode_with_csum(inode_ref);

        Ok(())
    }
}
