use super::{journal_transaction::Transaction, Ext4};
use crate::ext4_defs::{
    validate_xattr_block_for_release, xattr_block_checksum, AsBytes, InodeRef, SuperBlock,
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
        let bitmap = transaction.read(self.block_device.as_ref(), bitmap_block)?;
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

    /// Release the external xattr reference owned by an inode as part of the
    /// caller's final-reclaim transaction.
    ///
    /// A shared block is retained with a decremented reference count. An
    /// exclusively owned block is detached from the inode and returned to the
    /// caller, which must release it through the transaction allocator. This
    /// method never commits and never changes allocation metadata directly.
    pub(super) fn transaction_release_xattr(
        &self,
        transaction: &mut Transaction<'_>,
        inode: &mut InodeRef,
    ) -> Result<Option<PBlockId>> {
        let block_id = inode.inode.xattr_block();
        if block_id == 0 {
            return Ok(None);
        }
        let sb = self.read_super_block_cached();
        self.validate_xattr_block_allocation(transaction, block_id)?;
        let image = transaction.read(self.block_device.as_ref(), block_id)?;
        let (mut header, has_ea_inode) = validate_xattr_block_for_release(&*image)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        if has_ea_inode {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }

        // Linux verifies and updates h_checksum only with metadata_csum.
        const FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
        let checksum_seed = if sb.has_read_only_compatible_feature(FEATURE_RO_COMPAT_METADATA_CSUM)
        {
            let seed = sb.metadata_checksum_seed();
            let expected = xattr_block_checksum(seed, block_id, &*image)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            if header.checksum() != expected {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            Some(seed)
        } else {
            None
        };

        if header.refcount() > 1 {
            header.set_refcount(header.refcount() - 1);
            let image = self.transaction_block_for_update(transaction, block_id)?;
            if let Some(seed) = checksum_seed {
                header.set_checksum(0);
                image[..core::mem::size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
                let checksum = xattr_block_checksum(seed, block_id, image)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                header.set_checksum(checksum);
            }
            image[..core::mem::size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
            // This inode no longer owns a reference.  Detach it in the same
            // transaction as the shared reference-count decrement, otherwise
            // a restartable reclaim would decrement the block repeatedly.
            inode.inode.set_xattr_block(0);
            self.transaction_stage_inode_with_csum(transaction, inode)?;
            Ok(None)
        } else {
            inode.inode.set_xattr_block(0);
            self.transaction_stage_inode_with_csum(transaction, inode)?;
            Ok(Some(block_id))
        }
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
