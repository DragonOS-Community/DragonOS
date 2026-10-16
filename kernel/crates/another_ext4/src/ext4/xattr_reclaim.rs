use super::{
    journal_transaction::{BlockView, Transaction},
    Ext4,
};
use crate::ext4_defs::{
    validate_xattr_block_for_release, xattr_block_checksum, AsBytes, Block, InodeRef, SuperBlock,
    XattrHeader,
};
use crate::prelude::*;

impl Ext4 {
    fn validate_xattr_block_allocation(
        &self,
        transaction: &Transaction<'_>,
        block_id: PBlockId,
    ) -> Result<()> {
        let sb = self.read_super_block_cached();
        let first_data = sb.first_data_block() as PBlockId;
        let blocks_per_group = sb.blocks_per_group() as PBlockId;
        if blocks_per_group == 0 || block_id < first_data || block_id >= sb.block_count() {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        self.validate_data_blocks(block_id, 1)?;
        let relative = block_id - first_data;
        let group = relative / blocks_per_group;
        if group >= sb.block_group_count() as PBlockId {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let group_first = first_data
            .checked_add(
                group
                    .checked_mul(blocks_per_group)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
            )
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let valid_bits = core::cmp::min(
            blocks_per_group,
            sb.block_count()
                .checked_sub(group_first)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
        ) as usize;
        let bit = (block_id - group_first) as usize;
        if bit >= valid_bits {
            return Err(Ext4Error::new(ErrCode::EIO));
        }

        let bg = self.transaction_read_block_group(transaction, group as BlockGroupId)?;
        let bitmap_block = bg.desc.block_bitmap_block();
        if bitmap_block == 0 || bitmap_block >= sb.block_count() {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let bitmap = transaction.read(self, bitmap_block)?;
        if sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM) {
            let checksum_bytes = sb.clusters_per_group() as usize / 8;
            if !bg.verify_checksum(sb.metadata_checksum_seed())
                || !bg.desc.verify_block_bitmap_csum(
                    sb.metadata_checksum_seed(),
                    &*bitmap,
                    checksum_bytes,
                )
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
        }
        if !xattr_allocation_bit_is_set(&*bitmap, bit, valid_bits) {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        Ok(())
    }

    /// Validate a live external block before any ownership mutation. All
    /// callers hold the exclusive metadata gate, or run mount recovery alone.
    pub(super) fn transaction_read_xattr_block(
        &self,
        transaction: &Transaction<'_>,
        block_id: PBlockId,
    ) -> Result<(XattrHeader, Block)> {
        self.validate_xattr_block_allocation(transaction, block_id)?;
        let image = match transaction.read(self, block_id)? {
            BlockView::Device(block) => block,
            BlockView::Staged(bytes) => Block::new(block_id, try_copy_bytes(bytes)?),
        };
        let (header, has_ea_inode) = validate_xattr_block_for_release(&*image.data)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        if has_ea_inode {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let sb = self.read_super_block_cached();
        if sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM) {
            let expected =
                xattr_block_checksum(sb.metadata_checksum_seed(), block_id, &*image.data)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            if header.checksum() != expected {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
        }
        Ok((header, image))
    }

    /// Release an explicitly identified reference, without changing any inode.
    /// The returned exclusive block must be freed by the caller's allocator.
    /// Direct retirement calls this only after the old inode link is durable;
    /// journal callers stage it together with the inode detach.
    pub(super) fn transaction_release_xattr_block(
        &self,
        transaction: &mut Transaction<'_>,
        block_id: PBlockId,
    ) -> Result<Option<PBlockId>> {
        let (mut header, mut block) = self.transaction_read_xattr_block(transaction, block_id)?;
        if header.refcount() == 1 {
            return Ok(Some(block_id));
        }
        header.set_refcount(header.refcount() - 1);
        let sb = self.read_super_block_cached();
        let image = &mut *block.data;
        if sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM) {
            header.set_checksum(0);
            image[..core::mem::size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
            let checksum = xattr_block_checksum(sb.metadata_checksum_seed(), block_id, image)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            header.set_checksum(checksum);
        }
        image[..core::mem::size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
        transaction.stage(block_id, block.data)?;
        Ok(None)
    }

    /// Detach this inode and release its reference within one journal operation.
    pub(super) fn transaction_release_xattr(
        &self,
        transaction: &mut Transaction<'_>,
        inode: &mut InodeRef,
    ) -> Result<Option<PBlockId>> {
        let block_id = inode.inode.xattr_block();
        if block_id == 0 {
            return Ok(None);
        }
        let remaining_blocks = inode
            .inode
            .fs_block_count()
            .checked_sub(1)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let released = self.transaction_release_xattr_block(transaction, block_id)?;
        inode.inode.set_xattr_block(0);
        inode.inode.set_fs_block_count(remaining_blocks);
        self.transaction_stage_inode_with_csum(transaction, inode)?;
        Ok(released)
    }

    /// Retire an old reference after a durable Direct inode publication. Never
    /// detach the current inode here: it may already own a different home.
    pub(super) fn direct_release_xattr_block_locked(&self, block_id: PBlockId) -> Result<()> {
        let result = (|| {
            let mut transaction = self.transaction_start(6)?;
            if let Some(home) = self.transaction_release_xattr_block(&mut transaction, block_id)? {
                self.transaction_dealloc_block_range(&mut transaction, home, 1)?;
            }
            self.finish_direct_phase(transaction)
        })();
        if result.is_err() {
            // The inode already committed its detach. Even a preparation error
            // cannot restart the operation against its former ownership.
            self.poison(ErrCode::EIO);
        }
        result
    }

    /// Called under the existing exclusive gate by remove-last and final
    /// reclaim. Publish a durable detach before decrementing or freeing.
    pub(super) fn direct_detach_xattr_locked(&self, inode: &mut InodeRef) -> Result<()> {
        let home = inode.inode.xattr_block();
        if home == 0 {
            return Ok(());
        }
        if !self.block_device.supports_reliable_flush() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let remaining = inode
            .inode
            .fs_block_count()
            .checked_sub(1)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let mut detached = inode.clone();
        let mut transaction = self.transaction_start(1)?;
        self.transaction_read_xattr_block(&transaction, home)?;
        detached.inode.set_xattr_block(0);
        detached.inode.set_fs_block_count(remaining);
        self.transaction_stage_inode_with_csum(&mut transaction, &mut detached)?;
        self.finish_direct_phase(transaction)?;
        *inode = detached;
        self.direct_release_xattr_block_locked(home)
    }
}

fn xattr_allocation_bit_is_set(bitmap: &[u8], bit: usize, valid_bits: usize) -> bool {
    bit < valid_bits && bit / 8 < bitmap.len() && bitmap[bit / 8] & (1 << (bit % 8)) != 0
}

#[cfg(test)]
mod tests {
    use super::xattr_allocation_bit_is_set;

    #[test]
    fn free_or_out_of_range_xattr_block_is_rejected() {
        let bitmap = [0b0000_0100u8];
        assert!(xattr_allocation_bit_is_set(&bitmap, 2, 8));
        assert!(!xattr_allocation_bit_is_set(&bitmap, 3, 8));
        assert!(!xattr_allocation_bit_is_set(&bitmap, 8, 8));
    }
}
