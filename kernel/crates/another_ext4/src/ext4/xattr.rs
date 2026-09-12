//! Extended attribute access and atomic journal-mode replacement.
use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::prelude::*;
use crate::{format_error, return_error};

impl Ext4 {
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
    /// `Ok(value)` - the value of the attribute
    ///
    /// # Error
    ///
    /// `ENODATA` - the attribute does not exist
    pub fn getxattr(&self, inode: InodeId, name: &str) -> Result<Vec<u8>> {
        let _view = self.lock_metadata_read_view()?;
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        match xattr_block.get(name) {
            Some(value) => Ok(value.to_owned()),
            None => Err(format_error!(
                ErrCode::ENODATA,
                "Xattr {} does not exist",
                name
            )),
        }
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
    pub fn setxattr(&self, inode: InodeId, name: &str, value: &[u8]) -> Result<()> {
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
        name: &str,
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<()> {
        self.ensure_mutable()?;
        if self.uses_journal() {
            return self.update_xattr_transaction(inode, name, Some(value), create, replace);
        }
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode)].lock();
        let mut inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            if replace {
                return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
            }
            // lazy allocate xattr block
            let pblock = self.alloc_block(&mut inode_ref)?;
            let old_xattr_block = xattr_block_id;
            let result = (|| {
                let mut xattr_block = XattrBlock::new(self.read_block(pblock)?);
                xattr_block.init();
                if !xattr_block.insert(name, value) {
                    return_error!(
                        ErrCode::ENOSPC,
                        "Xattr block of Inode {} does not have enough space",
                        inode
                    );
                }
                self.update_xattr_block_checksum(pblock, &mut xattr_block)?;
                self.write_block(&xattr_block.block())?;
                inode_ref.inode.set_xattr_block(pblock);
                let blocks = inode_ref
                    .inode
                    .fs_block_count()
                    .checked_add(1)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
                inode_ref.inode.set_fs_block_count(blocks);
                self.write_inode_with_csum(&mut inode_ref)?;
                Ok(())
            })();
            if let Err(err) = result {
                inode_ref.inode.set_xattr_block(old_xattr_block);
                return match self.dealloc_block(&mut inode_ref, pblock) {
                    Ok(()) => Err(err),
                    Err(rollback_err) => Err(rollback_err),
                };
            }
            return Ok(());
        }

        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        let exists = xattr_block.get(name).is_some();
        if exists && create {
            return_error!(ErrCode::EEXIST, "Xattr {} already exists", name);
        }
        if !exists && replace {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }

        let mut new_xattr_block = xattr_block;
        if exists {
            let _ = new_xattr_block.remove(name);
        }
        if new_xattr_block.insert(name, value) {
            self.update_xattr_block_checksum(xattr_block_id, &mut new_xattr_block)?;
            self.write_block(&new_xattr_block.block())?;
            Ok(())
        } else {
            return_error!(
                ErrCode::ENOSPC,
                "Xattr block of Inode {} does not have enough space",
                inode
            );
        }
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
    pub fn removexattr(&self, inode: InodeId, name: &str) -> Result<()> {
        self.ensure_mutable()?;
        if self.uses_journal() {
            return self.update_xattr_transaction(inode, name, None, false, true);
        }
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode)].lock();
        let mut inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let image = self.read_block(xattr_block_id)?;
        let mut header = XattrHeader::from_bytes(&image.data[..]);
        let mut xattr_block = XattrBlock::new(image);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        if xattr_block.remove(name) {
            if xattr_block.list().is_empty() {
                let blocks = inode_ref
                    .inode
                    .fs_block_count()
                    .checked_sub(1)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                inode_ref.inode.set_xattr_block(0);
                inode_ref.inode.set_fs_block_count(blocks);
                self.write_inode_with_csum(&mut inode_ref)?;
                // In nojournal mode detach durably before returning this block
                // to the allocator; otherwise the old inode could reference a
                // newly reused block after a crash.
                if self.write_barrier {
                    self.block_device.flush()?;
                }
                let released = if header.refcount() == 1 {
                    self.dealloc_block(&mut inode_ref, xattr_block_id)
                } else {
                    // Detaching one owner must preserve the shared contents.
                    let mut original = self.read_block(xattr_block_id)?;
                    header.set_refcount(
                        header
                            .refcount()
                            .checked_sub(1)
                            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
                    );
                    original.data[..core::mem::size_of::<XattrHeader>()]
                        .copy_from_slice(header.to_bytes());
                    let mut original = XattrBlock::new(original);
                    self.update_xattr_block_checksum(xattr_block_id, &mut original)?;
                    self.write_block(&original.block())
                };
                if released.is_err() {
                    self.poison(ErrCode::EIO);
                }
                return released;
            }
            self.update_xattr_block_checksum(xattr_block_id, &mut xattr_block)?;
            self.write_block(&xattr_block.block())?;
            Ok(())
        } else {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
    }

    /// List extended attributes of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    ///
    /// # Returns
    ///
    /// A list of extended attributes of the file.
    pub fn listxattr(&self, inode: InodeId) -> Result<Vec<String>> {
        let _view = self.lock_metadata_read_view()?;
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return Ok(Vec::new());
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        self.verify_xattr_block_checksum(xattr_block_id, &xattr_block)?;
        Ok(xattr_block.list())
    }
    /// External xattr replacement is one allocation/reference/inode operation.
    /// Shared Linux xattr blocks are copied, never changed through another
    /// inode's reference. All fallible candidate construction precedes commit.
    fn update_xattr_transaction(
        &self,
        inode_id: InodeId,
        name: &str,
        value: Option<&[u8]>,
        create: bool,
        replace: bool,
    ) -> Result<()> {
        let _metadata = self.lock_transactional_metadata_mutation()?;
        let _inode = self.inode_mutation_locks[self.inode_mutation_lock_index(inode_id)].lock();
        let mut inode = self.read_inode(inode_id)?;
        let old_home = inode.inode.xattr_block();
        // At most: inode table, old/new xattr, bitmap, GDT and superblock.
        let mut transaction = self.transaction_start(6)?;
        let (mut candidate, shared) = if old_home == 0 {
            if replace || value.is_none() {
                return Err(Ext4Error::new(ErrCode::ENODATA));
            }
            let mut block = XattrBlock::new(Block::new(0, Box::new([0; BLOCK_SIZE])));
            block.init();
            (block, false)
        } else {
            let image = transaction.read(self, old_home)?;
            let (header, ea_inode) = validate_xattr_block_for_release(&*image)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            if ea_inode {
                return Err(Ext4Error::new(ErrCode::ENOTSUP));
            }
            let block = XattrBlock::new(Block::new(old_home, Box::new(*image)));
            self.verify_xattr_block_checksum(old_home, &block)?;
            (block, header.refcount() > 1)
        };
        let exists = candidate.get(name).is_some();
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
        if value.is_none() && candidate.list().is_empty() {
            if let Some(home) = self.transaction_release_xattr(&mut transaction, &mut inode)? {
                self.transaction_dealloc_block_range(&mut transaction, home, 1)?;
            }
            return self
                .commit_metadata_operation(transaction)
                .map(|_| ())
                .map_err(|error| error.error);
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
            .map_err(|error| error.error)
    }
}
