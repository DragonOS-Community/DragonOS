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
            let fblock = self.transaction_extent_query(transaction, dir, iblock)?;
            let view = transaction.read(self, fblock)?;
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

    /// Insert a name, including any directory/extent growth, in one private
    /// operation. Reuse the existing insertion and right-spine algorithms;
    /// none of the allocation or new directory contents reaches home early.
    pub(super) fn transaction_dir_add(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        dir: &mut InodeRef,
        child: &InodeRef,
        name: &str,
    ) -> Result<()> {
        match self.transaction_dir_add_existing(transaction, dir, child, name) {
            Ok(()) => return Ok(()),
            Err(error) if error.code() == ErrCode::ENOSPC => {}
            Err(error) => return Err(error),
        }
        let iblock = Self::dir_data_block_count(dir)?;
        let plan = self.transaction_right_spine_append_plan(transaction, dir, iblock)?;
        let home =
            self.transaction_alloc_metadata_block(transaction, dir.id, plan.preferred_first)?;
        let node_count = if plan.can_merge(home, 1) {
            0
        } else {
            plan.new_nodes()
        };
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(node_count)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        for _ in 0..node_count {
            nodes.push(self.transaction_alloc_metadata_block(transaction, dir.id, Some(home))?);
        }
        let mut block = DirBlock::new(Block::new(home, Box::new([0; BLOCK_SIZE])));
        block.init(self.metadata_csum_enabled());
        if !block.insert(
            name,
            child.id,
            child.inode.file_type(),
            self.metadata_csum_enabled(),
        ) {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        self.set_dir_block_checksum(dir, &mut block, DirBlockLayout::Leaf)?;
        transaction.stage(home, block.block().data.clone())?;
        self.stage_journaled_right_spine_append(transaction, dir, &plan, &nodes, home, 1)?;
        let size = dir
            .inode
            .size()
            .checked_add(BLOCK_SIZE as u64)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        dir.inode.set_size(size);
        self.transaction_stage_inode_with_csum(transaction, dir)
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
            let fblock = self.transaction_extent_query(transaction, dir, iblock)?;
            // Scanning must not consume a credit for every non-matching block
            // in a large directory. `read` still observes an already-staged
            // image if this helper is composed with another directory update.
            let view = transaction.read(self, fblock)?;
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
            let fblock = self.transaction_extent_query(transaction, dir, iblock)?;
            let view = transaction.read(self, fblock)?;
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
