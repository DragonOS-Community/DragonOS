use alloc::{boxed::Box, vec::Vec};

use crate::driver::base::block::block_device::BlockId;

use super::{BlockCacheError, BLOCK_SIZE};

/// # 枚举功能
/// 该枚举设计来是用于实现回写法的，但是目前并未使用
#[allow(dead_code)]
pub enum CacheBlockFlag {
    Unused,
    Unwrited,
    Writed,
}

pub type CacheBlockAddr = usize;

/// # 结构功能
/// 存储数据的最小单位
pub struct CacheBlock {
    data: Box<[u8]>,
    _flag: CacheBlockFlag,
    lba_id: BlockId,
}

impl CacheBlock {
    pub fn new(data: Box<[u8]>, flag: CacheBlockFlag, lba_id: BlockId) -> Self {
        CacheBlock {
            data,
            _flag: flag,
            lba_id,
        }
    }

    pub fn from_data(lba_id: BlockId, data: Vec<u8>) -> Self {
        let space_box = data.into_boxed_slice();
        CacheBlock::new(space_box, CacheBlockFlag::Unwrited, lba_id)
    }

    pub fn _set_flag(&mut self, _flag: CacheBlockFlag) -> Option<()> {
        todo!()
    }
    pub fn data(&self, buf: &mut [u8]) -> Result<usize, BlockCacheError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockCacheError::BlockSizeError);
        }
        buf.copy_from_slice(&self.data);
        return Ok(BLOCK_SIZE);
    }

    pub fn lba_id(&self) -> BlockId {
        self.lba_id
    }
}
