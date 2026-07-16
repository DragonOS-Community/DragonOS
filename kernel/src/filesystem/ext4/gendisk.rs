use alloc::boxed::Box;
use kdepends::another_ext4;
use system_error::SystemError;

use crate::driver::base::block::block_device::LBA_SIZE;
use crate::driver::base::block::gendisk::GenDisk;

const EXT4_BLOCKS_PER_BULK_WRITE: usize = 16;

impl GenDisk {
    fn convert_from_ext4_blkid(&self, ext4_blkid: u64) -> (usize, usize, usize) {
        // another_ext4 的逻辑块固定为 4096 字节（another_ext4::BLOCK_SIZE）。
        //
        // DragonOS 块设备的“LBA”语义固定为 512 字节（LBA_SIZE）。
        // GenDisk::block_offset_2_disk_blkid() 与 BlockDevice::read_at()/write_at()
        // 都是以 512B LBA 为单位进行寻址的。
        //
        // 因此这里必须把 ext4 的 4K block id 转成“512B LBA 偏移”，
        // 不能使用底层设备的 blk_size_log2（否则当设备上报 4K 物理块时会导致寻址单位混乱，
        // 进而读到错误数据，表现为 extent tree 解析失败/随机 ENOENT）。
        let blocks_per_ext4_block = another_ext4::BLOCK_SIZE / LBA_SIZE;
        let start_lba_offset = ext4_blkid as usize * blocks_per_ext4_block;
        let lba_id_start = self.block_offset_2_disk_blkid(start_lba_offset);
        let block_count = blocks_per_ext4_block;
        (start_lba_offset, lba_id_start, block_count)
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
        let lba_start = self
            .range()
            .lba_start
            .checked_add(relative_lba)
            .ok_or(SystemError::EOVERFLOW)?;
        let lba_end = lba_start
            .checked_add(lba_count)
            .ok_or(SystemError::EOVERFLOW)?;
        if lba_end > self.range().lba_end {
            return Err(SystemError::EFBIG);
        }
        Ok((lba_start, lba_count))
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
        let mut buf: Box<[u8; 4096]> = vec![0u8; another_ext4::BLOCK_SIZE]
            .into_boxed_slice()
            .try_into()
            .expect("Failed to convert boxed slice to boxed array");

        let (_, lba_id_start, block_count) = self.convert_from_ext4_blkid(block_id);
        self.block_device()
            .read_at(lba_id_start, block_count, &mut *buf)
            .map_err(|e| {
                log::error!("Ext4BlkDevice '{:?}' read_block failed: {:?}", block_id, e);
                another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&e))
            })?;
        Ok(another_ext4::Block::new(block_id, buf))
    }

    fn write_block(
        &self,
        block: &another_ext4::Block,
    ) -> core::result::Result<(), another_ext4::Ext4Error> {
        let (_, lba_id_start, block_count) = self.convert_from_ext4_blkid(block.id);
        self.block_device()
            .write_at(lba_id_start, block_count, &*block.data)
            .map_err(|e| {
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
            .chunks(EXT4_BLOCKS_PER_BULK_WRITE * another_ext4::BLOCK_SIZE)
            .enumerate()
        {
            let block_offset = chunk_index
                .checked_mul(EXT4_BLOCKS_PER_BULK_WRITE)
                .ok_or_else(|| another_ext4::Ext4Error::new(another_ext4::ErrCode::EFBIG))?;
            let chunk_start = start
                .checked_add(block_offset as u64)
                .ok_or_else(|| another_ext4::Ext4Error::new(another_ext4::ErrCode::EFBIG))?;
            let ext4_blocks = chunk.len() / another_ext4::BLOCK_SIZE;
            let (lba_start, lba_count) = self
                .checked_ext4_range(chunk_start, ext4_blocks)
                .map_err(|error| {
                    another_ext4::Ext4Error::new(Self::map_system_error_to_ext4(&error))
                })?;
            let completed = self
                .block_device()
                .write_at(lba_start, lba_count, chunk)
                .map_err(|error| {
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
        self.block_device().supports_reliable_flush()
    }
}
