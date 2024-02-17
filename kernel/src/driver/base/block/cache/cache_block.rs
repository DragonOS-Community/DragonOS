use core::cmp::Ordering;

use alloc::{boxed::Box, vec::Vec};

use super::cache_config::BLOCK_SIZE;
/// @brief 该枚举设计来是用于实现回写法的，但是目前并未使用
#[allow(dead_code)]
pub enum CacheBlockFlag {
    Unused,
    Unwrited,
    Writed,
}
#[derive(Copy, Clone)]
pub struct CacheBlockAddr(usize);

impl CacheBlockAddr {
    pub fn new(num: usize) -> Self {
        Self(num)
    }

    pub fn data(&self) -> usize {
        self.0
    }
}
impl PartialEq<usize> for CacheBlockAddr {
    fn eq(&self, other: &usize) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<usize> for CacheBlockAddr {
    fn partial_cmp(&self, other: &usize) -> Option<Ordering> {
        Some(self.0.cmp(other))
    }
}

/// @brief存储数据的最小单位
pub struct CacheBlock {
    data: Box<[u8]>,
    _flag: CacheBlockFlag,
    lba_id: usize,
}

impl CacheBlock {
    pub fn new(data: Box<[u8]>, flag: CacheBlockFlag, lba_id: usize) -> Self {
        CacheBlock {
            data,
            _flag: flag,
            lba_id,
        }
    }

    pub fn from_data(lba_id: usize, data: Vec<u8>) -> Self {
        let space_box = data.into_boxed_slice();
        CacheBlock::new(space_box, CacheBlockFlag::Unwrited, lba_id)
    }

    pub fn _set_flag(&mut self, _flag: CacheBlockFlag) -> Option<()> {
        todo!()
    }
    #[inline]
    pub fn get_data(&self, buf: &mut [u8]) -> usize {
        buf.copy_from_slice(&self.data);
        return BLOCK_SIZE;
    }

    pub fn get_lba_id(&self) -> usize {
        self.lba_id
    }
}
