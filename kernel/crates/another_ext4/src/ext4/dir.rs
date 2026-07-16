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
    fn validate_dir_name(name: &str) -> Result<()> {
        if name.len() > 255 {
            return_error!(ErrCode::ENAMETOOLONG, "Directory name exceeds 255 bytes");
        }
        Ok(())
    }

    fn validate_dir_block(
        &self,
        dir: &InodeRef,
        iblock: LBlockId,
        block: &DirBlock,
    ) -> Result<DirBlockLayout> {
        let sb = self.read_super_block_cached();
        block.validate(
            sb.metadata_checksum_seed(),
            dir.id,
            dir.inode.generation(),
            sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM),
            dir.inode.flags() & 0x1000 != 0,
            iblock == 0,
        )
    }

    fn metadata_csum_enabled(&self) -> bool {
        self.read_super_block_cached()
            .has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM)
    }

    fn set_dir_block_checksum(
        &self,
        dir: &InodeRef,
        block: &mut DirBlock,
        layout: DirBlockLayout,
    ) -> Result<()> {
        let sb = self.read_super_block_cached();
        if !sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM) {
            return Ok(());
        }
        match layout {
            DirBlockLayout::Leaf => {
                block.set_checksum(sb.metadata_checksum_seed(), dir.id, dir.inode.generation());
                Ok(())
            }
            DirBlockLayout::Htree => block.set_htree_checksum(
                sb.metadata_checksum_seed(),
                dir.id,
                dir.inode.generation(),
            ),
        }
    }

    fn dir_data_block_count(dir: &InodeRef) -> Result<u32> {
        let size = dir.inode.size();
        if !size.is_multiple_of(BLOCK_SIZE as u64) {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        u32::try_from(size / BLOCK_SIZE as u64).map_err(|_| Ext4Error::new(ErrCode::EIO))
    }

    pub(super) fn dir_has_insert_space(
        &self,
        dir: &InodeRef,
        child: &InodeRef,
        name: &str,
    ) -> Result<bool> {
        Self::validate_dir_name(name)?;
        for iblock in 0..Self::dir_data_block_count(dir)? {
            let fblock = self.extent_query(dir, iblock)?;
            let mut candidate = DirBlock::new(self.read_block(fblock)?);
            if self.validate_dir_block(dir, iblock, &candidate)? == DirBlockLayout::Htree {
                continue;
            }
            if candidate.insert(
                name,
                child.id,
                child.inode.file_type(),
                self.metadata_csum_enabled(),
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Prepare one checksum-valid empty directory block without publishing a
    /// name.  If initialization fails, the allocation is rolled back before
    /// any extent references it.  Once extent publication starts, failures are
    /// indeterminate and the caller must poison the mount.
    pub(super) fn prepare_empty_dir_slot(&self, dir: &mut InodeRef) -> Result<()> {
        let mut empty = DirBlock::new(Block::new(0, Box::new([0; BLOCK_SIZE])));
        let metadata_csum = self.metadata_csum_enabled();
        empty.init(metadata_csum);
        self.set_dir_block_checksum(dir, &mut empty, DirBlockLayout::Leaf)?;
        let iblock = self.extent_next_data_lblock(dir)?;
        let old_size = dir.inode.size();
        let old_blocks = dir.inode.fs_block_count();
        let new_size = old_size
            .checked_add(BLOCK_SIZE as u64)
            .ok_or_else(|| crate::format_error!(ErrCode::EFBIG, "Directory size overflow"))?;
        let new_blocks = old_blocks.checked_add(1).ok_or_else(|| {
            crate::format_error!(ErrCode::EFBIG, "Directory block count overflow")
        })?;
        dir.inode.set_size(new_size);
        dir.inode.set_fs_block_count(new_blocks);
        if let Err(error) = self.extent_query_or_create_initialized(
            dir,
            iblock,
            1,
            Some(empty.block().data.clone()),
        ) {
            dir.inode.set_size(old_size);
            dir.inode.set_fs_block_count(old_blocks);
            // Extent publication may have reached disk before reporting an
            // error; freeing pblock could create a dangling mapping.
            self.poison(ErrCode::EIO);
            return Err(error);
        }
        let total_blocks = match self.extent_all_data_blocks(dir).and_then(|data| {
            self.extent_all_tree_blocks(dir).and_then(|tree| {
                data.len().checked_add(tree.len()).ok_or_else(|| {
                    crate::format_error!(ErrCode::EFBIG, "Directory blocks overflow")
                })
            })
        }) {
            Ok(total) => total,
            Err(error) => {
                self.poison(ErrCode::EIO);
                return Err(error);
            }
        };
        dir.inode.set_fs_block_count(total_blocks as u64);
        if let Err(error) = self.write_inode_with_csum(dir) {
            self.poison(ErrCode::EIO);
            return Err(error);
        }
        Ok(())
    }

    /// Stage insertion into an existing directory data block.  The read-only
    /// scan consumes no journal credit; only the matching free-space block is
    /// copied into the transaction image.  Directory growth requires extent
    /// allocation and is intentionally rejected before any mutation until
    /// that allocation path is fully journalled.
    pub(super) fn transaction_dir_add_existing(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        dir: &InodeRef,
        child: &InodeRef,
        name: &str,
    ) -> Result<()> {
        Self::validate_dir_name(name)?;
        for iblock in 0..Self::dir_data_block_count(dir)? {
            let fblock = self.extent_query(dir, iblock)?;
            let view = transaction.read(self.block_device.as_ref(), fblock)?;
            let mut dir_block = DirBlock::new(Block::new(fblock, Box::new(*view)));
            let layout = self.validate_dir_block(dir, iblock, &dir_block)?;
            if layout == DirBlockLayout::Htree {
                continue;
            }
            if dir_block.insert(
                name,
                child.id,
                child.inode.file_type(),
                self.metadata_csum_enabled(),
            ) {
                self.set_dir_block_checksum(dir, &mut dir_block, layout)?;
                transaction.stage(fblock, dir_block.block().data.clone())?;
                return Ok(());
            }
        }
        return_error!(
            ErrCode::ENOSPC,
            "Atomic relink requires free space in directory {}",
            dir.id
        );
    }

    /// Find a directory entry that matches a given name under a parent directory
    pub(super) fn dir_find_entry(&self, dir: &InodeRef, name: &str) -> Result<InodeId> {
        Self::validate_dir_name(name)?;
        trace!("Dir find entry: dir {}, name {}", dir.id, name);
        let total_blocks = Self::dir_data_block_count(dir)?;
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            // Get the fs block id
            let fblock = self.extent_query(dir, iblock)?;
            // Load block from disk
            let dir_block = DirBlock::new(self.read_block(fblock)?);
            self.validate_dir_block(dir, iblock, &dir_block)?;
            // Find the entry in block
            let res = dir_block.get(name, self.metadata_csum_enabled());
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
        Self::validate_dir_name(name).map_err(DirAddFailure::Unmodified)?;
        trace!(
            "Dir add entry: dir {}, child {}, name {}",
            dir.id,
            child.id,
            name
        );
        let total_blocks = Self::dir_data_block_count(dir).map_err(DirAddFailure::Unmodified)?;
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
            let layout = self
                .validate_dir_block(dir, iblock, &dir_block)
                .map_err(DirAddFailure::Unmodified)?;
            if layout == DirBlockLayout::Htree {
                iblock += 1;
                continue;
            }
            // Try inserting the entry to parent block
            if dir_block.insert(
                name,
                child.id,
                child.inode.file_type(),
                self.metadata_csum_enabled(),
            ) {
                // Update checksum
                self.set_dir_block_checksum(dir, &mut dir_block, layout)
                    .map_err(DirAddFailure::Unmodified)?;
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
        new_dir_block.init(self.metadata_csum_enabled());
        new_dir_block.insert(
            name,
            child.id,
            child.inode.file_type(),
            self.metadata_csum_enabled(),
        );
        self.set_dir_block_checksum(dir, &mut new_dir_block, DirBlockLayout::Leaf)
            .map_err(DirAddFailure::Indeterminate)?;
        self.validate_dir_block(dir, iblock, &new_dir_block)
            .map_err(DirAddFailure::Indeterminate)?;
        // Write the block back to disk
        self.write_block(new_dir_block.block())
            .map_err(DirAddFailure::Indeterminate)?;
        self.write_inode_with_csum(dir)
            .map_err(DirAddFailure::Indeterminate)?;

        Ok(())
    }

    /// Remove a entry from a directory
    pub(super) fn dir_remove_entry(&self, dir: &InodeRef, name: &str) -> Result<()> {
        Self::validate_dir_name(name)?;
        trace!("Dir remove entry: dir {}, name {}", dir.id, name);
        let total_blocks = Self::dir_data_block_count(dir)?;
        // Check each block
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            // Get the parent physical block id
            let fblock = self.extent_query(dir, iblock)?;
            // Load the block from disk
            let mut dir_block = DirBlock::new(self.read_block(fblock)?);
            let layout = self.validate_dir_block(dir, iblock, &dir_block)?;
            if layout == DirBlockLayout::Htree {
                iblock += 1;
                continue;
            }
            // Try removing the entry
            if dir_block.remove(name, self.metadata_csum_enabled()) {
                // Update checksum
                self.set_dir_block_checksum(dir, &mut dir_block, layout)?;
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

    /// Stage removal of a directory entry in a transaction-private image.
    /// No namespace change becomes visible on disk or through caches until the
    /// caller commits the same transaction as its link-count updates.
    pub(super) fn transaction_dir_remove_entry(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        dir: &InodeRef,
        name: &str,
    ) -> Result<()> {
        Self::validate_dir_name(name)?;
        trace!(
            "Transaction dir remove entry: dir {}, name {}",
            dir.id,
            name
        );
        let total_blocks = Self::dir_data_block_count(dir)?;
        for iblock in 0..total_blocks {
            let fblock = self.extent_query(dir, iblock)?;
            // Scanning must not consume a credit for every non-matching block
            // in a large directory. `read` still observes an already-staged
            // image if this helper is composed with another directory update.
            let view = transaction.read(self.block_device.as_ref(), fblock)?;
            let mut dir_block = DirBlock::new(Block::new(fblock, Box::new(*view)));
            let layout = self.validate_dir_block(dir, iblock, &dir_block)?;
            if layout == DirBlockLayout::Htree {
                continue;
            }
            if dir_block.remove(name, self.metadata_csum_enabled()) {
                self.set_dir_block_checksum(dir, &mut dir_block, layout)?;
                transaction.stage(fblock, dir_block.block().data.clone())?;
                return Ok(());
            }
        }
        return_error!(
            ErrCode::ENOENT,
            "Directory entry not found: dir {}, name {}",
            dir.id,
            name
        );
    }

    /// Get all entries under a directory
    pub(super) fn dir_list_entries(&self, dir: &InodeRef) -> Result<Vec<DirEntry>> {
        let total_blocks = Self::dir_data_block_count(dir)?;
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            // Get the fs block id
            let fblock = self.extent_query(dir, iblock)?;
            // Load block from disk
            let dir_block = DirBlock::new(self.read_block(fblock)?);
            self.validate_dir_block(dir, iblock, &dir_block)?;
            // Get all entries from block
            dir_block.list(&mut entries, self.metadata_csum_enabled());
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
        Self::validate_dir_name(name)?;
        trace!(
            "Dir replace entry: dir {}, name {}, new_inode {}",
            dir.id,
            name,
            new_inode
        );
        let total_blocks = Self::dir_data_block_count(dir)?;
        let mut iblock: LBlockId = 0;
        while iblock < total_blocks {
            let fblock = self.extent_query(dir, iblock)?;
            let mut dir_block = DirBlock::new(self.read_block(fblock)?);
            let layout = self.validate_dir_block(dir, iblock, &dir_block)?;
            if dir_block.replace(name, new_inode, new_type, self.metadata_csum_enabled()) {
                self.set_dir_block_checksum(dir, &mut dir_block, layout)?;
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

    /// Stage an in-place directory-entry replacement in the caller's
    /// transaction (the JBD2 equivalent of Linux `ext4_setent()`).
    ///
    /// The directory scan is read-only and therefore consumes no journal
    /// credit for non-matching blocks.  Only the block containing `name` is
    /// staged; if another rename operation already staged that same physical
    /// block, `Transaction::stage` replaces the transaction-private image
    /// without consuming a second credit.
    pub(super) fn transaction_dir_replace_entry(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        dir: &InodeRef,
        name: &str,
        new_inode: InodeId,
        new_type: FileType,
    ) -> Result<()> {
        Self::validate_dir_name(name)?;
        trace!(
            "Transaction dir replace entry: dir {}, name {}, new_inode {}",
            dir.id,
            name,
            new_inode
        );
        let total_blocks = Self::dir_data_block_count(dir)?;
        for iblock in 0..total_blocks {
            let fblock = self.extent_query(dir, iblock)?;
            let view = transaction.read(self.block_device.as_ref(), fblock)?;
            let mut dir_block = DirBlock::new(Block::new(fblock, Box::new(*view)));
            let layout = self.validate_dir_block(dir, iblock, &dir_block)?;
            if dir_block.replace(name, new_inode, new_type, self.metadata_csum_enabled()) {
                self.set_dir_block_checksum(dir, &mut dir_block, layout)?;
                transaction.stage(fblock, dir_block.block().data.clone())?;
                return Ok(());
            }
        }
        return_error!(
            ErrCode::ENOENT,
            "Directory entry not found for transactional replace: dir {}, name {}",
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
