use alloc::sync::Arc;
use kdepends::another_ext4;
use system_error::SystemError;

use crate::driver::base::block::gendisk::GenDisk;
use crate::driver::base::block::{bio::BioRequest, block_device::LBA_SIZE};

pub(super) const EXT4_BLOCKS_PER_BULK_IO: usize = 16;

/// One accepted contiguous ext4 read. The BIO and its DMA storage stay owned
/// until `wait_read_into` observes device retirement.
pub(super) struct SubmittedExt4Read {
    bio: Arc<BioRequest>,
    expected_bytes: usize,
}

impl SubmittedExt4Read {
    pub(super) fn wait_read_into(self, dst: &mut [u8]) -> Result<(), SystemError> {
        if dst.len() != self.expected_bytes {
            return Err(SystemError::EINVAL);
        }
        self.bio.wait_read_into(dst)?;
        Ok(())
    }
}

impl GenDisk {
    pub(super) fn submit_ext4_read(
        &self,
        start: u64,
        ext4_block_count: usize,
    ) -> Result<SubmittedExt4Read, SystemError> {
        if ext4_block_count == 0 || ext4_block_count > EXT4_BLOCKS_PER_BULK_IO {
            return Err(SystemError::EINVAL);
        }
        let expected_bytes = ext4_block_count
            .checked_mul(another_ext4::BLOCK_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let (lba_start, lba_count) = self.checked_ext4_range(start, ext4_block_count)?;
        let bio = self.submit_read(lba_start, lba_count)?;
        Ok(SubmittedExt4Read {
            bio,
            expected_bytes,
        })
    }

    fn checked_ext4_range(
        &self,
        ext4_blkid: u64,
        ext4_block_count: usize,
    ) -> Result<(usize, usize), SystemError> {
        if ext4_block_count == 0 {
            return Err(SystemError::EINVAL);
        }
        let blocks_per_ext4_block = another_ext4::BLOCK_SIZE / LBA_SIZE;
        let ext4_blkid = usize::try_from(ext4_blkid).map_err(|_| SystemError::EOVERFLOW)?;
        let relative_lba = ext4_blkid
            .checked_mul(blocks_per_ext4_block)
            .ok_or(SystemError::EOVERFLOW)?;
        let lba_count = ext4_block_count
            .checked_mul(blocks_per_ext4_block)
            .ok_or(SystemError::EOVERFLOW)?;
        let relative_end = relative_lba
            .checked_add(lba_count)
            .ok_or(SystemError::EOVERFLOW)?;
        if relative_end > self.range().len() {
            return Err(SystemError::EFBIG);
        }
        Ok((relative_lba, lba_count))
    }

    fn map_system_error_to_ext4(e: &SystemError) -> another_ext4::ErrCode {
        match e {
            SystemError::EROFS => another_ext4::ErrCode::EROFS,
            SystemError::ENOSPC => another_ext4::ErrCode::ENOSPC,
            SystemError::ENOENT => another_ext4::ErrCode::ENOENT,
            SystemError::ENOTDIR => another_ext4::ErrCode::ENOTDIR,
            SystemError::EISDIR => another_ext4::ErrCode::EISDIR,
            SystemError::EINVAL => another_ext4::ErrCode::EINVAL,
            SystemError::ENOMEM => another_ext4::ErrCode::ENOMEM,
            SystemError::EFBIG | SystemError::EOVERFLOW => another_ext4::ErrCode::EFBIG,
            SystemError::EIO => another_ext4::ErrCode::EIO,
            _ => another_ext4::ErrCode::EIO,
        }
    }
}

impl another_ext4::BlockDevice for GenDisk {
    // - convert the ext4 block id to gendisk block id
    // - read the block from gendisk
    // - return the block
    fn read_block(
        &self,
        block_id: u64,
    ) -> core::result::Result<another_ext4::Block, another_ext4::Ext4Error> {
        let mut buf = another_ext4::Block::try_zeroed_data()?;

        let (lba_start, _) = self.checked_ext4_range(block_id, 1).map_err(|error| {
            another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&error))
        })?;
        let completed = self.read_at(&mut *buf, lba_start).map_err(|e| {
            log::error!("Ext4BlkDevice '{:?}' read_block failed: {:?}", block_id, e);
            another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&e))
        })?;
        if completed != another_ext4::BLOCK_SIZE {
            return Err(another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO));
        }
        Ok(another_ext4::Block::new(block_id, buf))
    }

    fn read_blocks(
        &self,
        start: u64,
        data: &mut [u8],
    ) -> core::result::Result<(), another_ext4::Ext4Error> {
        if data.is_empty() || !data.len().is_multiple_of(another_ext4::BLOCK_SIZE) {
            return Err(another_ext4::Ext4Error::new(another_ext4::ErrCode::EINVAL));
        }

        for (chunk_index, chunk) in data
            .chunks_mut(EXT4_BLOCKS_PER_BULK_IO * another_ext4::BLOCK_SIZE)
            .enumerate()
        {
            let block_offset = chunk_index
                .checked_mul(EXT4_BLOCKS_PER_BULK_IO)
                .ok_or_else(|| another_ext4::Ext4Error::new(another_ext4::ErrCode::EFBIG))?;
            let chunk_start = start
                .checked_add(block_offset as u64)
                .ok_or_else(|| another_ext4::Ext4Error::new(another_ext4::ErrCode::EFBIG))?;
            let ext4_blocks = chunk.len() / another_ext4::BLOCK_SIZE;
            let (lba_start, _) =
                self.checked_ext4_range(chunk_start, ext4_blocks)
                    .map_err(|error| {
                        another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&error))
                    })?;
            let completed = self.read_at(chunk, lba_start).map_err(|error| {
                another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&error))
            })?;
            if completed != chunk.len() {
                return Err(another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO));
            }
        }
        Ok(())
    }

    fn write_block(
        &self,
        block: &another_ext4::Block,
    ) -> core::result::Result<(), another_ext4::Ext4Error> {
        let (lba_start, _) = self.checked_ext4_range(block.id, 1).map_err(|error| {
            another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&error))
        })?;
        let completed = self.write_at(&*block.data, lba_start).map_err(|e| {
            let code = Self::map_system_error_to_ext4(&e);
            if code == another_ext4::ErrCode::EROFS {
                log::trace!(
                    "Ext4BlkDevice '{:?}' write_block on readonly media: {:?}",
                    block.id,
                    e
                );
            } else {
                log::error!("Ext4BlkDevice '{:?}' write_block failed: {:?}", block.id, e);
            }
            another_ext4::Ext4Error::new(code)
        })?;
        if completed != another_ext4::BLOCK_SIZE {
            return Err(another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO));
        }
        Ok(())
    }

    fn write_blocks(
        &self,
        start: u64,
        data: &[u8],
    ) -> core::result::Result<(), another_ext4::Ext4Error> {
        if data.is_empty() || !data.len().is_multiple_of(another_ext4::BLOCK_SIZE) {
            return Err(another_ext4::Ext4Error::new(another_ext4::ErrCode::EINVAL));
        }

        for (chunk_index, chunk) in data
            .chunks(EXT4_BLOCKS_PER_BULK_IO * another_ext4::BLOCK_SIZE)
            .enumerate()
        {
            let block_offset = chunk_index
                .checked_mul(EXT4_BLOCKS_PER_BULK_IO)
                .ok_or_else(|| another_ext4::Ext4Error::new(another_ext4::ErrCode::EFBIG))?;
            let chunk_start = start
                .checked_add(block_offset as u64)
                .ok_or_else(|| another_ext4::Ext4Error::new(another_ext4::ErrCode::EFBIG))?;
            let ext4_blocks = chunk.len() / another_ext4::BLOCK_SIZE;
            let (lba_start, _) =
                self.checked_ext4_range(chunk_start, ext4_blocks)
                    .map_err(|error| {
                        another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&error))
                    })?;
            let completed = self.write_at(chunk, lba_start).map_err(|error| {
                another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&error))
            })?;
            if completed != chunk.len() {
                return Err(another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO));
            }
        }
        Ok(())
    }

    fn flush(&self) -> core::result::Result<(), another_ext4::Ext4Error> {
        self.sync().map_err(|e| {
            log::error!("Ext4BlkDevice flush failed: {:?}", e);
            another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&e))
        })
    }

    fn supports_reliable_flush(&self) -> bool {
        self.block_device()
            .is_ok_and(|bdev| bdev.supports_reliable_flush())
    }
}
