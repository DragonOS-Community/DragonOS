use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_error;

pub(super) enum DirAddFailure {
    Unmodified(Ext4Error),
    Indeterminate(Ext4Error),
}

impl DirAddFailure {
    pub(super) fn into_error(self) -> Ext4Error {
        match self {
            Self::Unmodified(error) | Self::Indeterminate(error) => error,
        }
    }
}

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
            let dir_block = DirBlock::new(self.read_block(fblock)?);
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
        self.dir_add_entry_classified(dir, child, name)
            .map_err(DirAddFailure::into_error)
    }

    /// Add a directory entry while preserving whether a failure happened
    /// before any metadata mutation.  Namespace transactions use this to avoid
    /// fail-stopping on a clean ENOSPC while still treating post-allocation I/O
    /// failures as indeterminate.
    pub(super) fn dir_add_entry_classified(
        &self,
        dir: &mut InodeRef,
        child: &InodeRef,
        name: &str,
    ) -> core::result::Result<(), DirAddFailure> {
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
            let fblock = self
                .extent_query(dir, iblock)
                .map_err(DirAddFailure::Unmodified)?;
            // Load the parent block from disk
            let mut dir_block =
                DirBlock::new(self.read_block(fblock).map_err(DirAddFailure::Unmodified)?);
            // Try inserting the entry to parent block
            if dir_block.insert(name, child.id, child.inode.file_type()) {
                // Update checksum
                dir_block.set_checksum(
                    &self.read_super_block_cached().uuid(),
                    dir.id,
                    dir.inode.generation(),
                );
                // Write the block back to disk
                self.write_block(dir_block.block())
                    .map_err(DirAddFailure::Indeterminate)?;
                return Ok(());
            }
            // Current block has no enough space
            iblock += 1;
        }
        // A full filesystem is a clean, expected failure at this boundary: no
        // extent or directory metadata has been changed yet.
        if self.read_super_block_cached().free_blocks_count() == 0 {
            return Err(DirAddFailure::Unmodified(crate::format_error!(
                ErrCode::ENOSPC,
                "No free block available to extend directory {}",
                dir.id
            )));
        }

        // From the first allocation onward, an error can follow a partial
        // extent-tree or counter update and is therefore indeterminate.
        // Append a new data block
        let (_, fblock) = self
            .inode_append_block(dir)
            .map_err(DirAddFailure::Indeterminate)?;
        // Update inode size
        dir.inode.set_size(dir.inode.size() + BLOCK_SIZE as u64);
        // Load new block
        let mut new_dir_block = DirBlock::new(
            self.read_block(fblock)
                .map_err(DirAddFailure::Indeterminate)?,
        );
        // Write the entry to block
        new_dir_block.init();
        new_dir_block.insert(name, child.id, child.inode.file_type());
        new_dir_block.set_checksum(
            &self.read_super_block_cached().uuid(),
            dir.id,
            dir.inode.generation(),
        );
        // Write the block back to disk
        self.write_block(new_dir_block.block())
            .map_err(DirAddFailure::Indeterminate)?;

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
            let fblock = self.extent_query(dir, iblock)?;
            // Load the block from disk
            let mut dir_block = DirBlock::new(self.read_block(fblock)?);
            // Try removing the entry
            if dir_block.remove(name) {
                // Update checksum
                dir_block.set_checksum(
                    &self.read_super_block_cached().uuid(),
                    dir.id,
                    dir.inode.generation(),
                );
                // Write the block back to disk
                self.write_block(dir_block.block())?;
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
    pub(super) fn dir_list_entries(&self, dir: &InodeRef) -> Result<Vec<DirEntry>> {
        let total_blocks = dir.inode.fs_block_count() as u32;
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            // Get the fs block id
            let fblock = self.extent_query(dir, iblock)?;
            // Load block from disk
            let dir_block = DirBlock::new(self.read_block(fblock)?);
            // Get all entries from block
            dir_block.list(&mut entries);
            iblock += 1;
        }
        Ok(entries)
    }

    /// Replace a directory entry's inode in place.
    /// Used for atomic rename when target exists (equivalent to Linux ext4_setent).
    pub(super) fn dir_replace_entry(
        &self,
        dir: &InodeRef,
        name: &str,
        new_inode: InodeId,
        new_type: FileType,
    ) -> Result<()> {
        trace!(
            "Dir replace entry: dir {}, name {}, new_inode {}",
            dir.id,
            name,
            new_inode
        );
        let total_blocks = dir.inode.fs_block_count() as u32;
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            let fblock = self.extent_query(dir, iblock)?;
            let mut dir_block = DirBlock::new(self.read_block(fblock)?);
            if dir_block.replace(name, new_inode, new_type) {
                dir_block.set_checksum(
                    &self.read_super_block_cached().uuid(),
                    dir.id,
                    dir.inode.generation(),
                );
                self.write_block(dir_block.block())?;
                return Ok(());
            }
            iblock += 1;
        }
        return_error!(
            ErrCode::ENOENT,
            "Directory entry not found for replace: dir {}, name {}",
            dir.id,
            name
        );
    }

    /// Check if a directory is empty (only contains "." and "..")
    pub(super) fn dir_is_empty(&self, dir: &InodeRef) -> Result<bool> {
        let entries = self.dir_list_entries(dir)?;
        let res = entries.iter().all(|e| {
            let name = e.name();
            name == "." || name == ".."
        });
        Ok(res)
    }
}
