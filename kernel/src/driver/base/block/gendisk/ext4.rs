use alloc::boxed::Box;

impl super::GenDisk {
    fn convert_from_ext4_blkid(&self, ext4_blkid: u64) -> (usize, usize, usize) {
        let start_block_offset =
            ext4_blkid as usize * (another_ext4::BLOCK_SIZE / (1 << self.block_size_log2 as usize));
        let lba_id_start = self.block_offset_2_disk_blkid(start_block_offset);
        let block_count = another_ext4::BLOCK_SIZE / (1 << self.block_size_log2 as usize);
        (start_block_offset, lba_id_start, block_count)
    }
}

impl another_ext4::BlockDevice for super::GenDisk {
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
            .expect("read block error");
        another_ext4::Block::new(block_id, buf)
    }

    fn write_block(&self, block: &another_ext4::Block) {
        let (_, lba_id_start, block_count) = self.convert_from_ext4_blkid(block.id);
        self.block_device()
            .write_at(lba_id_start, block_count, &*block.data)
            .expect("write block error");
    }
}
