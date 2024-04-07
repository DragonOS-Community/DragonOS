use crate::driver::base::block::block_device::BlockId;

/// # 结构功能
/// 一个简单的结构体，是BlockIter的输出
#[derive(Debug)]
pub struct BlockData {
    /// 表示单个块对应的lba_id
    lba_id: BlockId,
    /// 表示该块在buf中的起始地址，目前并没有作用（例如：若该块是第2个块，那么该数据成员值为2*BLOCK_SIZE）
    _data_start_addr: BlockId,
    /// 表示该块的大小
    _block_size: usize,
}

impl BlockData {
    pub fn new(lba_id: BlockId, data_start_addr: BlockId, block_size: usize) -> Self {
        Self {
            lba_id,
            _data_start_addr: data_start_addr,
            _block_size: block_size,
        }
    }
    #[inline]
    pub fn lba_id(&self) -> BlockId {
        self.lba_id
    }
    #[inline]
    pub fn _data_start_addr(&self) -> BlockId {
        self._data_start_addr
    }
    #[inline]
    pub fn _block_size(&self) -> usize {
        self._block_size
    }
}

/// # 结构功能
/// 块迭代器，它获取需求（起始块，连续块的个数），并将连续的块输出为单一的块（如你需要读取lba_id为10~20的连续块，它就可以输出10,11...,20的BlockData）
#[derive(Copy, Clone)]
pub struct BlockIter {
    /// 表示起始块的lba_id
    lba_id_start: BlockId,
    /// 表示从起始块开始你需要读多少个块
    count: usize,
    /// 表示当前遍历到第几个块了
    current: usize,
    /// 规定块的大小
    block_size: usize,
}

impl BlockIter {
    pub fn new(lba_id_start: BlockId, count: usize, block_size: usize) -> Self {
        Self {
            lba_id_start,
            count,
            block_size,
            current: 0,
        }
    }
}

impl Iterator for BlockIter {
    type Item = BlockData;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current < self.count {
            let ans = BlockData::new(
                self.lba_id_start + self.current,
                self.current * self.block_size,
                self.block_size,
            );
            self.current += 1;
            Some(ans)
        } else {
            None
        }
    }
}

/// # 结构功能
/// 表示缺块信息的数据结构，往往在读取的时候发现缺块并产生FailData，在插入的时候使用FailData
pub struct FailData {
    /// 表示缺块的lba_id
    lba_id: BlockId,
    /// 表示缺块在buf中的位置，用于在insert的时候定位缺块数据的位置
    index: usize,
}

impl FailData {
    pub fn new(lba_id: BlockId, index: usize) -> Self {
        FailData { lba_id, index }
    }
    #[inline]
    pub fn lba_id(&self) -> BlockId {
        self.lba_id
    }
    /// # 函数的功能
    /// 该函数返回的是缺块在buf中的位置，比如：index=1，那么我们就应该取buf\[512..1024\]
    pub fn index(&self) -> usize {
        self.index
    }
}
