use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_error;

impl Ext4 {
    /// Find a directory entry that matches a given name under a parent directory
    pub(super) fn dir_find_entry(&self, dir: &InodeRef, name: &str) -> Result<InodeId> {
        trace!("Dir find entry: dir {}, name {}", dir.id, name);
        let total_blocks = dir.inode.fs_block_count() as u32;
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            // Get the fs block id
            let fblock = self.extent_query(dir, iblock)?;
            // Load block from disk
            let dir_block = DirBlock::new(self.read_block(fblock));
            // Find the entry in block
            let res = dir_block.get(name);
            if let Some(r) = res {
                return Ok(r);
            }
            iblock += 1;
        }
        return_error!(
            ErrCode::ENOENT,
            "Directory entry not found: dir {}, name {}",
            dir.id,
            name
        );
    }

    /// Add an entry to a directory, memory consistency guaranteed
    pub(super) fn dir_add_entry(
        &self,
        dir: &mut InodeRef,
        child: &InodeRef,
        name: &str,
    ) -> Result<()> {
        trace!(
            "Dir add entry: dir {}, child {}, name {}",
            dir.id,
            child.id,
            name
        );
        let total_blocks = dir.inode.fs_block_count() as u32;
        let mut iblock: LBlockId = 0;
        // Try finding a block with enough space
        while iblock < total_blocks {
            // Get the parent physical block id
            let fblock = self.extent_query(dir, iblock).unwrap();
            // Load the parent block from disk
            let mut dir_block = DirBlock::new(self.read_block(fblock));
            // Try inserting the entry to parent block
            if dir_block.insert(name, child.id, child.inode.file_type()) {
                // Update checksum
                dir_block.set_checksum(
                    &self.read_super_block().uuid(),
                    dir.id,
                    dir.inode.generation(),
                );
                // Write the block back to disk
                self.write_block(dir_block.block());
                return Ok(());
            }
            // Current block has no enough space
            iblock += 1;
        }
        // No free block found - needed to allocate a new data block
        // Append a new data block
        let (_, fblock) = self.inode_append_block(dir)?;
        // Update inode size
        dir.inode.set_size(dir.inode.size() + BLOCK_SIZE as u64);
        // Load new block
        let mut new_dir_block = DirBlock::new(self.read_block(fblock));
        // Write the entry to block
        new_dir_block.init();
        new_dir_block.insert(name, child.id, child.inode.file_type());
        new_dir_block.set_checksum(
            &self.read_super_block().uuid(),
            dir.id,
            dir.inode.generation(),
        );
        // Write the block back to disk
        self.write_block(new_dir_block.block());

        Ok(())
    }

    /// Remove a entry from a directory
    pub(super) fn dir_remove_entry(&self, dir: &InodeRef, name: &str) -> Result<()> {
        trace!("Dir remove entry: dir {}, name {}", dir.id, name);
        let total_blocks = dir.inode.fs_block_count() as u32;
        // Check each block
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            // Get the parent physical block id
            let fblock = self.extent_query(dir, iblock).unwrap();
            // Load the block from disk
            let mut dir_block = DirBlock::new(self.read_block(fblock));
            // Try removing the entry
            if dir_block.remove(name) {
                // Update checksum
                dir_block.set_checksum(
                    &self.read_super_block().uuid(),
                    dir.id,
                    dir.inode.generation(),
                );
                // Write the block back to disk
                self.write_block(dir_block.block());
                return Ok(());
            }
            // Current block has no enough space
            iblock += 1;
        }
        // Not found the target entry
        return_error!(
            ErrCode::ENOENT,
            "Directory entry not found: dir {}, name {}",
            dir.id,
            name
        );
    }

    /// Get all entries under a directory
    pub(super) fn dir_list_entries(&self, dir: &InodeRef) -> Vec<DirEntry> {
        let total_blocks = dir.inode.fs_block_count() as u32;
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            // Get the fs block id
            let fblock = self.extent_query(dir, iblock).unwrap();
            // Load block from disk
            let dir_block = DirBlock::new(self.read_block(fblock));
            // Get all entries from block
            dir_block.list(&mut entries);
            iblock += 1;
        }
        entries
    }
}
