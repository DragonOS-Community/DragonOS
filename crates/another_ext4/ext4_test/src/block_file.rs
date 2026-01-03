use another_ext4::{Block, BlockDevice, BLOCK_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

#[derive(Debug)]
pub struct BlockFile(File);

impl BlockFile {
    pub fn new(path: &str) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        Self(file)
    }
}

impl BlockDevice for BlockFile {
    fn read_block(&self, block_id: u64) -> Block {
        let mut file = &self.0;
        let mut buffer = [0u8; BLOCK_SIZE];
        // warn!("read_block {}", block_id);
        let _r = file.seek(SeekFrom::Start(block_id * BLOCK_SIZE as u64));
        let _r = file.read_exact(&mut buffer);
        Block::new(block_id, buffer)
    }

    fn write_block(&self, block: &Block) {
        let mut file = &self.0;
        // warn!("write_block {}", block.block_id);
        let _r = file.seek(SeekFrom::Start(block.id * BLOCK_SIZE as u64));
        let _r = file.write_all(&block.data);
    }
}
