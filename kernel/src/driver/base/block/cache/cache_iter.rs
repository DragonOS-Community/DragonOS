#[derive(Debug)]

pub struct BlockData {
    iba_id: usize,
    _data_start_addr: usize,
    _block_size: usize,
}

impl BlockData {
    pub fn new(iba_id: usize, data_start_addr: usize, block_size: usize) -> Self {
        Self {
            iba_id,
            _data_start_addr: data_start_addr,
            _block_size: block_size,
        }
    }
    #[inline]
    pub fn iba_id(&self) -> usize {
        self.iba_id
    }
    #[inline]
    pub fn _data_start_addr(&self) -> usize {
        self._data_start_addr
    }
    #[inline]
    pub fn _block_size(&self) -> usize {
        self._block_size
    }
}
#[derive(Copy, Clone)]
pub struct BlockIter {
    iba_id_start: usize,
    count: usize,
    current: usize,
    block_size: usize,
}

impl BlockIter {
    pub fn new(lba_id_start: usize, count: usize, block_size: usize) -> Self {
        Self {
            iba_id_start: lba_id_start,
            count,
            block_size,
            current: 0,
        }
    }
}

impl Iterator for BlockIter {
    type Item = BlockData;

    // 定义 next 方法，返回 Option<Self::Item>
    fn next(&mut self) -> Option<Self::Item> {
        if self.current < self.count {
            let ans = BlockData::new(
                self.iba_id_start + self.current,
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

pub struct FailData {
    lba_id: usize,
    index: usize,
}

impl FailData {
    pub fn new(lba_id: usize, index: usize) -> Self {
        FailData { lba_id, index }
    }
    #[inline]
    pub fn lba_id(&self) -> usize {
        self.lba_id
    }
///@brief 该函数返回的是缺块在buf中的位置，比如：index=1，那么我们就应该取buf\[512..1024\]
    pub fn index(&self) -> usize {
        self.index
    }
}
