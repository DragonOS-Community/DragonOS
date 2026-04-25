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
    fn read_block(&self, block_id: u64) -> core::result::Result<Block, another_ext4::Ext4Error> {
        let mut file = &self.0;
        let mut buffer = [0u8; BLOCK_SIZE];
        // warn!("read_block {}", block_id);
        file.seek(SeekFrom::Start(block_id * BLOCK_SIZE as u64))
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        file.read_exact(&mut buffer)
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        Ok(Block::new(block_id, Box::new(buffer)))
    }

    fn write_block(&self, block: &Block) -> core::result::Result<(), another_ext4::Ext4Error> {
        let mut file = &self.0;
        // warn!("write_block {}", block.block_id);
        file.seek(SeekFrom::Start(block.id * BLOCK_SIZE as u64))
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        file.write_all(&*block.data)
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        Ok(())
    }
}
