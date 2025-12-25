use alloc::boxed::Box;
use kdepends::another_ext4;
use system_error::SystemError;

use crate::driver::base::block::block_device::LBA_SIZE;
use crate::driver::base::block::gendisk::GenDisk;

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
}

impl another_ext4::BlockDevice for GenDisk {
    // - convert the ext4 block id to gendisk block id
    // - read the block from gendisk
    // - return the block
    fn read_block(&self, block_id: u64) -> another_ext4::Block {
        let mut buf: Box<[u8; 4096]> = vec![0u8; another_ext4::BLOCK_SIZE]
            .into_boxed_slice()
            .try_into()
            .expect("Failed to convert boxed slice to boxed array");

        let (_, lba_id_start, block_count) = self.convert_from_ext4_blkid(block_id);
        self.block_device()
            .read_at(lba_id_start, block_count, &mut *buf)
            .map_err(|e| {
                log::error!("Ext4BlkDevice '{:?}' read_block failed: {:?}", block_id, e);
                SystemError::EIO
            })
            .unwrap();
        another_ext4::Block::new(block_id, buf)
    }

    fn write_block(&self, block: &another_ext4::Block) {
        let (_, lba_id_start, block_count) = self.convert_from_ext4_blkid(block.id);
        self.block_device()
            .write_at(lba_id_start, block_count, &*block.data)
            .map_err(|e| {
                log::error!("Ext4BlkDevice '{:?}' write_block failed: {:?}", block.id, e);
                SystemError::EIO
            })
            .unwrap();
    }
}
