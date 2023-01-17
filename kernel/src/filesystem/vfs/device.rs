/// 该文件定义了 Device 和 BlockDevice 的接口
/// Notice 设备错误码使用 Posix 规定的 int32_t 的错误码表示，而不是自己定义错误enum

/// 引入Module
use crate::include::bindings::bindings::E2BIG;

/// 定义类型
pub type BlockId = usize;

/// 定义常量
const BLK_SIZE_LOG2_LIMIT: u8 = 12; // 设定块设备的块大小不能超过 1 << 12.

/// @brief 设备应该实现的操作
/// @usage Device::read_at()
pub trait Device: Send + Sync {
    /// Notice buffer对应设备按字节划分，使用u8类型
    /// Notice offset应该从0开始计数
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32>;
    fn write_at(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, i32>;
    fn sync(&self) -> Result<(), i32>;
}

/// @brief 块设备应该实现的操作
pub trait BlockDevice: Send + Sync {
    const BLK_SIZE_LOG2: u8; // 需要保证块设备的块大小是2的幂次
    fn read_at(&self, lba_id_start: BlockId, count: usize, buf: &mut [u8]) -> Result<(), i32>;
    fn write_at(&self, lba_id_start: BlockId, count: usize, buf: &[u8]) -> Result<(), i32>;
    fn sync(&self) -> Result<(), i32>;
}

/// 对于所有块设备自动实现 Device Trait 的 read_at 和 write_at 函数
impl<T: BlockDevice> Device for T {
    // 读取设备操作，读取设备内部 [offset, offset + buf.len) 区间内的字符，存放到 buf 中
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32> {
        // assert!(len <= buf.len());
        if len > buf.len() {
            return Err(-(E2BIG as i32));
        }

        let iter = BlockIter::new_multiblock(offset, offset + len, Self::BLK_SIZE_LOG2);
        let multi = iter.multiblock;

        // 枚举每一个range
        for range in iter {
            let buf_begin = range.origin_begin() - offset; // 本次读操作的起始位置/已经读了这么多字节
            let buf_end = range.origin_end() - offset;
            let buf_slice = &mut buf[buf_begin..buf_end];
            let count: usize = (range.lba_end - range.lba_start).try_into().unwrap();
            let full = multi && range.is_multi() || !multi && range.is_full();

            if full {
                // 调用 BlockDevice::read_at() 直接把引用传进去，不是把整个数组move进去
                BlockDevice::read_at(self, range.lba_start, count, buf_slice)?;
            } else {
                // 判断块的长度不能超过最大值
                if Self::BLK_SIZE_LOG2 > BLK_SIZE_LOG2_LIMIT {
                    return Err(-(E2BIG as i32));
                }

                let mut temp = Vec::new();
                temp.resize(1usize << Self::BLK_SIZE_LOG2, 0);
                BlockDevice::read_at(self, range.lba_start, 1, &mut temp[..])?;
                // 把数据从临时buffer复制到目标buffer
                buf_slice.copy_from_slice(&temp[range.begin..range.end]);
            }
        }
        return Ok(len);
    }

    /// 写入设备操作，把 buf 的数据写入到设备内部 [offset, offset + len) 区间内
    fn write_at(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, i32> {
        // assert!(len <= buf.len());
        if len > buf.len() {
            return Err(-(E2BIG as i32));
        }

        let iter = BlockIter::new_multiblock(offset, offset + len, Self::BLK_SIZE_LOG2);
        let multi = iter.multiblock;

        for range in iter {
            let buf_begin = range.origin_begin() - offset; // 本次读操作的起始位置/已经读了这么多字节
            let buf_end = range.origin_end() - offset;
            let buf_slice = &buf[buf_begin..buf_end];
            let count: usize = (range.lba_end - range.lba_start).try_into().unwrap();
            let full = multi && range.is_multi() || !multi && range.is_full();

            if full {
                BlockDevice::write_at(self, range.lba_start, count, buf_slice)?;
            } else {
                if Self::BLK_SIZE_LOG2 > BLK_SIZE_LOG2_LIMIT {
                    return Err(-(E2BIG as i32));
                }

                let mut temp = Vec::new();
                temp.resize(1usize << Self::BLK_SIZE_LOG2, 0);
                // 由于块设备每次读写都是整块的，在不完整写入之前，必须把不完整的地方补全
                BlockDevice::read_at(self, range.lba_start, 1, &mut temp[..])?;
                // 把数据从临时buffer复制到目标buffer
                temp[range.begin..range.end].copy_from_slice(&buf_slice);
                BlockDevice::write_at(self, range.lba_start, 1, &temp[..])?;
            }
        }
        return Ok(len);
    }

    /// 数据同步
    fn sync(&self) -> Result<(), i32> {
        BlockDevice::sync(self)
    }
}

/// @brief 块设备的迭代器
/// @usage 某次操作读/写块设备的[L,R]范围内的字节，
///        那么可以使用此结构体进行迭代遍历，每次调用next()返回一个BlockRange
pub struct BlockIter {
    pub begin: usize, // 迭代器的起始位置 -> 块设备的地址 （单位是字节）
    pub end: usize,
    pub blk_size_log2: u8,
    pub multiblock: bool, // 是否启用连续整块同时遍历
}

/// @brief Range搭配迭代器BlockIter使用，[L,R]区间被分割成多个小的Range
///        Range要么是整块，要么是一块的某一部分
/// 细节： range = [begin, end) 左闭右开
pub struct BlockRange {
    pub lba_start: usize, // 起始块的lba_id
    pub lba_end: usize,   // 终止块的lba_id
    pub begin: usize, // 起始位置在块内的偏移量， 如果BlockIter启用Multiblock，则是多个块的偏移量
    pub end: usize,   // 结束位置在块内的偏移量，单位是字节
    pub blk_size_log2: u8,
}

impl BlockIter {
    pub fn new(start_addr: usize, end_addr: usize, blk_size_log2: u8) -> BlockIter {
        return BlockIter {
            begin: start_addr,
            end: end_addr,
            blk_size_log2: blk_size_log2,
            multiblock: false,
        };
    }
    pub fn new_multiblock(start_addr: usize, end_addr: usize, blk_size_log2: u8) -> BlockIter {
        return BlockIter {
            begin: start_addr,
            end: end_addr,
            blk_size_log2: blk_size_log2,
            multiblock: true,
        };
    }

    /// 获取下一个整块或者不完整的块
    pub fn next_block(&mut self) -> BlockRange {
        let blk_size_log2 = self.blk_size_log2;
        let blk_size = 1usize << self.blk_size_log2;
        let lba_id = self.begin / blk_size;
        let begin = self.begin % blk_size;
        let end = if lba_id == self.end / blk_size {
            self.end % blk_size
        } else {
            blk_size
        };

        self.begin += end - begin;

        return BlockRange {
            lba_start: lba_id,
            lba_end: lba_id + 1,
            begin: begin,
            end: end,
            blk_size_log2: blk_size_log2,
        };
    }

    /// 如果能返回多个连续的整块，则返回；否则调用next_block()返回不完整的块
    pub fn next_multiblock(&mut self) -> BlockRange {
        let blk_size_log2 = self.blk_size_log2;
        let blk_size = 1usize << self.blk_size_log2;
        let lba_start = self.begin / blk_size;
        let lba_end = self.end / blk_size;

        // 如果不是整块，先返回非整块的小部分
        if addr_to_lba_id(self.begin, blk_size)
            != addr_to_lba_id(self.begin + blk_size - 1, blk_size)
            || lba_start == lba_end
        {
            return self.next_block();
        }

        let begin = self.begin % blk_size; // 因为是多个整块，这里必然是0
        let end = lba_id_to_addr(lba_end, blk_size) - self.begin;
        // assert!(begin == 0);
        // assert!(end % blk_size == 0);

        self.begin += end - begin;

        return BlockRange {
            lba_start: lba_start,
            lba_end: lba_end,
            begin: begin,
            end: end,
            blk_size_log2: blk_size_log2,
        };
    }
}

/// BlockIter 函数实现
impl Iterator for BlockIter {
    type Item = BlockRange;

    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        if self.begin >= self.end {
            return None;
        }
        if self.multiblock {
            return Some(self.next_multiblock());
        } else {
            return Some(self.next_block());
        }
    }
}

/// BlockRange 函数实现
impl BlockRange {
    pub fn is_empty(&self) -> bool {
        return self.end == self.begin;
    }
    pub fn len(&self) -> usize {
        return self.end - self.begin;
    }
    /// 判断是不是整块
    pub fn is_full(&self) -> bool {
        return self.len() == (1usize << self.blk_size_log2);
    }
    /// 判断是不是多个整块连在一起
    pub fn is_multi(&self) -> bool {
        return self.len() >= (1usize << self.blk_size_log2)
            && (self.len() % (1usize << self.blk_size_log2) == 0);
    }
    /// 获取 BlockRange 在块设备内部的起始位置 (单位是字节)
    pub fn origin_begin(&self) -> usize {
        return (self.lba_start << self.blk_size_log2) + self.begin;
    }
    /// 获取 BlockRange 在块设备内部的结尾位置 (单位是字节)
    pub fn origin_end(&self) -> usize {
        return (self.lba_start << self.blk_size_log2) + self.end;
    }
}

/// 从字节地址转换到lba id
#[inline]
fn addr_to_lba_id(addr: usize, blk_size: usize) -> BlockId {
    return addr / blk_size;
}

/// 从lba id转换到字节地址
#[inline]
fn lba_id_to_addr(lba_id: usize, blk_size: usize) -> BlockId {
    return lba_id * blk_size;
}
