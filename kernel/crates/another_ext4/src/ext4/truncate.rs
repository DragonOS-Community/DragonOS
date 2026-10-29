//! A syscall-local size publication receipt. It is not a persistent writer
//! owner: recovery authority remains the existing legacy orphan chain.
use super::{orphan::LegacyOrphanMembership, Ext4, SetAttr};
use crate::{constants::*, ext4_defs::*, prelude::*};

pub struct StartedSizeChange<'a> {
    fs: &'a Ext4,
    id: InodeId,
    generation: u32,
    links: u16,
    size: u64,
    cleanup: bool,
    remove_orphan: bool,
    active: bool,
}

impl Drop for StartedSizeChange<'_> {
    fn drop(&mut self) {
        if self.active {
            self.fs.fail_stop_mutations();
        }
    }
}

impl StartedSizeChange<'_> {
    pub fn needs_cleanup(&self) -> bool {
        self.cleanup
    }
}

impl Ext4 {
    /// Pure field merge shared by Journal/Batched and ordered Direct helpers.
    pub(in crate::ext4) fn apply_setattr_fields(inode: &mut InodeRef, attr: &SetAttr) {
        if let Some(value) = attr.mode {
            inode.inode.set_mode(value);
        }
        if let Some(value) = attr.uid {
            inode.inode.set_uid(value);
        }
        if let Some(value) = attr.gid {
            inode.inode.set_gid(value);
        }
        if let Some(value) = attr.size {
            inode.inode.set_size(value);
        }
        if let Some(value) = attr.atime {
            inode.inode.set_atime(value);
        }
        if let Some(value) = attr.mtime {
            inode.inode.set_mtime(value);
        }
        if let Some(value) = attr.ctime {
            inode.inode.set_ctime(value);
        }
        if let Some(value) = attr.crtime {
            inode.inode.set_crtime(value);
        }
    }

    /// Publish SIZE/attributes before cache discard. The caller supplies its
    /// stable visible EOF; it must retain truncate/link exclusion until finish.
    pub fn begin_size_change(
        &self,
        id: InodeId,
        previous_visible_size: u64,
        attr: &SetAttr,
    ) -> Result<StartedSizeChange<'_>> {
        let size = attr.size.ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
        if size.div_ceil(BLOCK_SIZE as u64) > u64::from(MAX_BLOCKS) {
            return Err(Ext4Error::new(ErrCode::EFBIG));
        }
        self.ensure_mutable()?;
        let _metadata = self.lock_transactional_metadata_mutation()?;
        let _mutation = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        self.begin_size_change_locked(id, previous_visible_size, attr)
    }

    pub(super) fn begin_size_change_locked(
        &self,
        id: InodeId,
        previous_visible_size: u64,
        attr: &SetAttr,
    ) -> Result<StartedSizeChange<'_>> {
        let size = attr.size.ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
        if size.div_ceil(BLOCK_SIZE as u64) > u64::from(MAX_BLOCKS) {
            return Err(Ext4Error::new(ErrCode::EFBIG));
        }
        let mut inode = self.read_inode_uncached(id)?;
        if !inode.inode.is_file() || !inode.inode.uses_extents() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let cleanup = size <= previous_visible_size.max(inode.inode.size());
        let membership = self.legacy_orphan_membership(&inode)?;
        if inode.inode.link_count() == 0 && membership != LegacyOrphanMembership::ZeroLink {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let remove_orphan = cleanup && inode.inode.link_count() != 0;
        let enroll = remove_orphan && membership == LegacyOrphanMembership::Absent;
        if !cleanup {
            // Raw callers have no cached dirty edge; native callers have
            // already persisted their prepared old-EOF prefix through bridge.
            self.zero_tail_at_locked(&inode, previous_visible_size.max(inode.inode.size()), None)?;
        }
        let mut durable_intent = membership == LegacyOrphanMembership::LinkedTail;
        let result = (|| {
            if self.uses_journal() {
                let mut transaction = self.transaction_start(2)?;
                if enroll {
                    let mut sb = self.transaction_read_super_block(&transaction)?;
                    self.transaction_ensure_linked_tail_orphan(
                        &mut transaction,
                        &mut inode,
                        &mut sb,
                    )?;
                }
                Self::apply_setattr_fields(&mut inode, attr);
                self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
                self.finish_range_transaction(transaction)
            } else {
                if enroll {
                    self.direct_enroll_linked_tail_locked(&mut inode)?;
                    durable_intent = true;
                }
                self.direct_publish_size_attrs_locked(&mut inode, attr)
            }
        })();
        if let Err(error) = result {
            // Existing cleanup intent or a possibly durable new enrollment
            // must never be released to ordinary writers without a receipt.
            if durable_intent {
                self.fail_stop_mutations();
            }
            return Err(error);
        }
        Ok(StartedSizeChange {
            fs: self,
            id,
            generation: inode.inode.generation(),
            links: inode.inode.link_count(),
            size,
            cleanup,
            remove_orphan,
            active: true,
        })
    }

    /// The optional edge image preserves the upper cache's latest dirty prefix.
    /// None is for raw callers/recovery which own only durable disk contents.
    pub fn finish_size_change(
        &self,
        receipt: &mut StartedSizeChange<'_>,
        edge_image: Option<&[u8; BLOCK_SIZE]>,
    ) -> Result<()> {
        if !core::ptr::eq(self, receipt.fs) || !receipt.active {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.ensure_mutable()?;
        let _metadata = self.lock_transactional_metadata_mutation()?;
        let _mutation =
            self.inode_mutation_locks[self.inode_mutation_lock_index(receipt.id)].lock();
        self.finish_size_change_locked(receipt, edge_image)
    }

    pub(super) fn finish_size_change_locked(
        &self,
        receipt: &mut StartedSizeChange<'_>,
        edge_image: Option<&[u8; BLOCK_SIZE]>,
    ) -> Result<()> {
        let mut inode = self.read_inode_uncached(receipt.id)?;
        if inode.inode.generation() != receipt.generation
            || inode.inode.size() != receipt.size
            || inode.inode.link_count() != receipt.links
        {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        if receipt.cleanup {
            self.zero_truncate_tail_locked(&inode, edge_image)?;
            let first = receipt.size.div_ceil(BLOCK_SIZE as u64) as LBlockId;
            self.punch_block_range_locked(receipt.id, first, MAX_BLOCKS)?;
            if receipt.remove_orphan {
                inode = self.read_inode_uncached(receipt.id)?;
                self.finish_linked_truncate_orphan_locked(&mut inode)?;
            }
        }
        receipt.active = false;
        Ok(())
    }

    pub(super) fn zero_truncate_tail_locked(
        &self,
        inode: &InodeRef,
        edge_image: Option<&[u8; BLOCK_SIZE]>,
    ) -> Result<()> {
        self.zero_tail_at_locked(inode, inode.inode.size(), edge_image)
    }

    fn zero_tail_at_locked(
        &self,
        inode: &InodeRef,
        eof: u64,
        edge_image: Option<&[u8; BLOCK_SIZE]>,
    ) -> Result<()> {
        if eof.div_ceil(BLOCK_SIZE as u64) > u64::from(MAX_BLOCKS) {
            return Err(Ext4Error::new(ErrCode::EFBIG));
        }
        let within = (eof % BLOCK_SIZE as u64) as usize;
        if within == 0 {
            return Ok(());
        }
        let logical = (eof / BLOCK_SIZE as u64) as LBlockId;
        let Some(extent) = self.allocated_extent_at_or_after(inode, logical, None)? else {
            return Ok(());
        };
        if extent.start_lblock() > logical || extent.is_unwritten() {
            return Ok(());
        }
        if !self.block_device.supports_reliable_flush() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let home = extent
            .start_pblock()
            .checked_add(u64::from(logical - extent.start_lblock()))
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        self.validate_data_blocks(home, 1)?;
        let mut block = self.block_device.read_block(home)?;
        if let Some(image) = edge_image {
            block.data[..within].copy_from_slice(&image[..within]);
        }
        block.data[within..].fill(0);
        self.block_device.write_block(&block)?;
        self.block_device.flush()
    }

    pub(super) fn finish_linked_truncate_orphan_locked(&self, inode: &mut InodeRef) -> Result<()> {
        if !self.uses_journal() {
            return self.direct_unlink_tail_orphan_locked(inode);
        }
        let mut transaction = self.transaction_start(8)?;
        let mut sb = self.transaction_read_super_block(&transaction)?;
        self.transaction_orphan_del(&mut transaction, inode, &mut sb)?;
        inode.inode.set_next_orphan(0);
        self.transaction_stage_inode_with_csum(&mut transaction, inode)?;
        self.finish_range_transaction(transaction)
    }
}
