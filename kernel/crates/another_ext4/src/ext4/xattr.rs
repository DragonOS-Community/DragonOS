//! External xattr access, journal replacement and ordered nojournal ownership.
use super::{journal_transaction::Transaction, Ext4, MetadataMutationMode};
use crate::ext4_defs::*;
use crate::prelude::*;

impl Ext4 {
    /// Keep the inode's external block alive through its complete read. Direct
    /// final detach uses the exclusive side of this same gate; a per-inode
    /// snapshot alone cannot prevent the block from being freed and reused.
    fn lock_xattr_read_view(
        &self,
    ) -> Result<Option<super::metadata_gate::MetadataMutationGuard<'_>>> {
        if matches!(self.metadata_mode, MetadataMutationMode::Direct(_)) {
            self.metadata_mutation_barrier.try_read_view().map(Some)
        } else {
            self.lock_metadata_read_view()
        }
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

    /// Get extended attribute of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    /// * `name` - the name of the attribute
    ///
    /// # Return
    ///
    /// Return the value length. An empty buffer probes the size; a nonempty
    /// buffer must fit the whole value and is left unchanged on ERANGE.
    ///
    /// # Error
    ///
    /// `ENODATA` - the attribute does not exist
    pub fn getxattr(&self, inode: InodeId, name: &[u8], buf: &mut [u8]) -> Result<usize> {
        XattrEntry::validate_name(name)?;
        let _view = self.lock_xattr_read_view()?;
        let inode_ref = self.read_inode(inode)?;
        let home = inode_ref.inode.xattr_block();
        if home == 0 {
            return Err(Ext4Error::new(ErrCode::ENODATA));
        }
        let block = XattrBlock::new(self.read_block(home)?);
        self.verify_xattr_block_checksum(home, &block)?;
        let value = block
            .get(name)?
            .ok_or_else(|| Ext4Error::new(ErrCode::ENODATA))?;
        if !buf.is_empty() {
            if buf.len() < value.len() {
                return Err(Ext4Error::new(ErrCode::ERANGE));
            }
            buf[..value.len()].copy_from_slice(value);
        }
        Ok(value.len())
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
    pub fn setxattr(&self, inode: InodeId, name: &[u8], value: &[u8]) -> Result<()> {
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
        name: &[u8],
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<()> {
        self.ensure_mutable()?;
        XattrEntry::validate_name(name)?;
        self.update_xattr(inode, name, Some(value), create, replace)
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
    pub fn removexattr(&self, inode: InodeId, name: &[u8]) -> Result<()> {
        self.ensure_mutable()?;
        XattrEntry::validate_name(name)?;
        self.update_xattr(inode, name, None, false, true)
    }

    /// List extended attributes of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    ///
    /// # Returns
    ///
    /// The length of the NUL-separated byte names. An empty buffer probes
    /// the size; copying uses the same checked block snapshot.
    pub fn listxattr(&self, inode: InodeId, buf: &mut [u8]) -> Result<usize> {
        let _view = self.lock_xattr_read_view()?;
        let inode_ref = self.read_inode(inode)?;
        let home = inode_ref.inode.xattr_block();
        if home == 0 {
            return Ok(0);
        }
        let block = XattrBlock::new(self.read_block(home)?);
        self.verify_xattr_block_checksum(home, &block)?;
        block.list(buf)
    }
    /// External xattr replacement is one allocation/reference/inode operation.
    /// Shared Linux xattr blocks are copied, never changed through another
    /// inode's reference. All fallible candidate construction precedes commit.
    fn update_xattr(
        &self,
        inode_id: InodeId,
        name: &[u8],
        value: Option<&[u8]>,
        create: bool,
        replace: bool,
    ) -> Result<()> {
        let _metadata = self.lock_transactional_metadata_mutation()?;
        let _inode = self.inode_mutation_locks[self.inode_mutation_lock_index(inode_id)].lock();
        let mut inode = self.read_inode(inode_id)?;
        let old_home = inode.inode.xattr_block();
        if old_home != 0 && inode.inode.fs_block_count() == 0 {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        // At most: inode table, old/new xattr, bitmap, GDT and superblock.
        let mut transaction = self.transaction_start(6)?;
        let (mut candidate, shared) = if old_home == 0 {
            if replace || value.is_none() {
                return Err(Ext4Error::new(ErrCode::ENODATA));
            }
            let mut block = XattrBlock::new(Block::new(0, try_filled_bytes(0)?));
            block.init();
            (block, false)
        } else {
            let (header, image) = self.transaction_read_xattr_block(&transaction, old_home)?;
            let block = XattrBlock::new(image);
            (block, header.refcount() > 1)
        };
        candidate.validate_inline()?;
        let exists = candidate.get(name)?.is_some();
        if exists && create {
            return Err(Ext4Error::new(ErrCode::EEXIST));
        }
        if !exists && (replace || value.is_none()) {
            return Err(Ext4Error::new(ErrCode::ENODATA));
        }
        if exists {
            candidate.remove(name);
        }
        if let Some(value) = value {
            if !candidate.insert(name, value) {
                return Err(Ext4Error::new(ErrCode::ENOSPC));
            }
        }
        let empty = value.is_none() && candidate.is_empty()?;
        if !self.uses_journal() {
            if empty {
                transaction.abort();
                return self.direct_detach_xattr_locked(&mut inode);
            }
            return self.update_xattr_direct(transaction, inode, candidate, shared);
        }
        if empty {
            if let Some(home) = self.transaction_release_xattr(&mut transaction, &mut inode)? {
                self.transaction_dealloc_block_range(&mut transaction, home, 1)?;
            }
            return self
                .commit_metadata_operation(transaction)
                .map(|_| ())
                .map_err(|error| {
                    if error.poisoned {
                        self.poison(ErrCode::EIO);
                    }
                    error.error
                });
        }
        let home = if old_home == 0 || shared {
            let home = self.transaction_alloc_metadata_block(&mut transaction, inode_id, None)?;
            if shared {
                self.transaction_release_xattr(&mut transaction, &mut inode)?;
            }
            let blocks = inode
                .inode
                .fs_block_count()
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            inode.inode.set_fs_block_count(blocks);
            inode.inode.set_xattr_block(home);
            self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
            home
        } else {
            old_home
        };
        let mut image = candidate.block();
        image.id = home;
        let mut header = XattrHeader::from_bytes(&image.data[..]);
        header.set_refcount(1);
        image.data[..core::mem::size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
        let mut candidate = XattrBlock::new(image);
        self.update_xattr_block_checksum(home, &mut candidate)?;
        transaction.stage(home, candidate.block().data)?;
        self.commit_metadata_operation(transaction)
            .map(|_| ())
            .map_err(|error| {
                if error.poisoned {
                    self.poison(ErrCode::EIO);
                }
                error.error
            })
    }

    /// The caller holds the exclusive metadata gate and inode shard. Only a
    /// new home changes ownership; private, nonempty updates stay in place.
    fn update_xattr_direct(
        &self,
        mut ownership: Transaction<'_>,
        mut inode: InodeRef,
        candidate: XattrBlock,
        shared: bool,
    ) -> Result<()> {
        let old_home = inode.inode.xattr_block();
        let needs_home = old_home == 0 || shared;
        if needs_home && !self.block_device.supports_reliable_flush() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let blocks = if old_home == 0 {
            inode
                .inode
                .fs_block_count()
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?
        } else {
            inode.inode.fs_block_count()
        };
        let home = if needs_home {
            self.transaction_alloc_metadata_block(&mut ownership, inode.id, None)?
        } else {
            old_home
        };
        let mut image = candidate.block();
        image.id = home;
        let mut header = XattrHeader::from_bytes(&image.data[..]);
        header.set_refcount(1);
        image.data[..core::mem::size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
        let mut candidate = XattrBlock::new(image);
        self.update_xattr_block_checksum(home, &mut candidate)?;
        let candidate = candidate.block();
        if !needs_home {
            ownership.stage(home, candidate.data)?;
            return self
                .commit_metadata_operation(ownership)
                .map(|_| ())
                .map_err(|error| {
                    if error.poisoned {
                        self.poison(ErrCode::EIO);
                    }
                    error.error
                });
        }

        // Prepare the complete new inode image before committing any claims.
        // The exclusive gate also protects other inodes in this table block.
        inode.inode.set_xattr_block(home);
        inode.inode.set_fs_block_count(blocks);
        self.transaction_stage_inode_with_csum(&mut ownership, &mut inode)?;
        let inode_home = self.inode_disk_pos(inode.id)?.0;
        let inode_image = ownership
            .take_direct_image(inode_home)?
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;

        // A: allocation ownership first, while the old inode still owns its
        // old home. A failed phase is uncertain and already fail-stops.
        self.finish_direct_phase(ownership)?;
        let before_inode = (|| {
            // B: complete, checksum-correct new image must be durable before
            // any inode can make it reachable.
            self.block_device.write_block(&candidate)?;
            self.block_device.flush()?;
            let mut root = self.transaction_start(1)?;
            root.stage(inode_home, inode_image)?;
            Ok(root)
        })();
        let root = match before_inode {
            Ok(root) => root,
            Err(error) => {
                // No inode write was attempted: the new claim cannot be live.
                let rollback = (|| {
                    let mut transaction = self.transaction_start(3)?;
                    self.transaction_dealloc_block_range(&mut transaction, home, 1)?;
                    self.finish_direct_phase(transaction)
                })();
                if let Err(rollback_error) = rollback {
                    self.poison(ErrCode::EIO);
                    return Err(rollback_error);
                }
                return Err(error);
            }
        };
        // C: an attempted inode write/flush can already have published the new
        // home. Never roll back its claim after this boundary.
        self.finish_direct_phase(root)?;
        // D: use the explicit OLD home, not the now-published inode pointer.
        if old_home != 0 {
            self.direct_release_xattr_block_locked(old_home)?;
        }
        Ok(())
    }
}
