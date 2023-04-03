/// 引入Module
use crate::{syscall::SystemError};
use alloc::{sync::Arc, vec::Vec};
use core::{any::Any, fmt::Debug};

use super::disk_info::Partition;

/// 该文件定义了 Device 和 BlockDevice 的接口
/// Notice 设备错误码使用 Posix 规定的 int32_t 的错误码表示，而不是自己定义错误enum

// 使用方法:
// 假设 blk_dev 是块设备
// <blk_dev as Device>::read_at() 调用的是Device的函数
// <blk_dev as BlockDevice>::read_at() 调用的是BlockDevice的函数

/// 定义类型
pub type BlockId = usize;

/// 定义常量
const BLK_SIZE_LOG2_LIMIT: u8 = 12; // 设定块设备的块大小不能超过 1 << 12.
/// 在DragonOS中，我们认为磁盘的每个LBA大小均为512字节。（注意，文件系统的1个扇区可能事实上是多个LBA）
pub const LBA_SIZE: usize = 512;

/// @brief 设备应该实现的操作
/// @usage Device::read_at()
pub trait Device: Any + Send + Sync + Debug {
    /// Notice buffer对应设备按字节划分，使用u8类型
    /// Notice offset应该从0开始计数

    /// @brief: 从设备的第offset个字节开始，读取len个byte，存放到buf中
    /// @parameter offset: 起始字节偏移量
    /// @parameter len: 读取字节的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回操作的长度(单位是字节)；否则返回错误码；如果操作异常，但是并没有检查出什么错误，将返回已操作的长度
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, SystemError>;

    /// @brief: 从设备的第offset个字节开始，把buf数组的len个byte，写入到设备中
    /// @parameter offset: 起始字节偏移量
    /// @parameter len: 读取字节的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回操作的长度(单位是字节)；否则返回错误码；如果操作异常，但是并没有检查出什么错误，将返回已操作的长度
    fn write_at(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, SystemError>;

    /// @brief: 同步信息，把所有的dirty数据写回设备 - 待实现
    fn sync(&self) -> Result<(), SystemError>;

    // TODO: 待实现 open, close

}

/// @brief 块设备应该实现的操作
pub trait BlockDevice: Any + Send + Sync + Debug {
    /// @brief: 在块设备中，从第lba_id_start个块开始，读取count个块数据，存放到buf中
    ///
    /// @parameter lba_id_start: 起始块
    /// @parameter count: 读取块的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回 Ok(操作的长度) 其中单位是字节；
    ///          否则返回Err(错误码)，其中错误码为负数；
    ///          如果操作异常，但是并没有检查出什么错误，将返回Err(已操作的长度)
    fn read_at(&self, lba_id_start: BlockId, count: usize, buf: &mut [u8]) -> Result<usize, SystemError>;

    /// @brief: 在块设备中，从第lba_id_start个块开始，把buf中的count个块数据，存放到设备中
    /// @parameter lba_id_start: 起始块
    /// @parameter count: 写入块的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回 Ok(操作的长度) 其中单位是字节；
    ///          否则返回Err(错误码)，其中错误码为负数；
    ///          如果操作异常，但是并没有检查出什么错误，将返回Err(已操作的长度)
    fn write_at(&self, lba_id_start: BlockId, count: usize, buf: &[u8]) -> Result<usize, SystemError>;

    /// @brief: 同步磁盘信息，把所有的dirty数据写回硬盘 - 待实现
    fn sync(&self) -> Result<(), SystemError>;

    /// @breif: 每个块设备都必须固定自己块大小，而且该块大小必须是2的幂次
    /// @return: 返回一个固定量，硬编码(编程的时候固定的常量).
    fn blk_size_log2(&self) -> u8;

    // TODO: 待实现 open, close

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;

    /// @brief 本函数用于将BlockDevice转换为Device。
    /// 由于实现了BlockDevice的结构体，本身也实现了Device Trait, 因此转换是可能的。
    /// 思路：在BlockDevice的结构体中新增一个self_ref变量，返回self_ref.upgrade()即可。
    fn device(&self) -> Arc<dyn Device>;

    /// @brief 返回块设备的块大小（单位：字节）
    fn block_size(&self) -> usize;

    /// @brief 返回当前磁盘上的所有分区的Arc指针数组
    fn partitions(&self) -> Vec<Arc<Partition>>;
}

/// 对于所有<块设备>自动实现 Device Trait 的 read_at 和 write_at 函数
impl<T: BlockDevice> Device for T {
    // 读取设备操作，读取设备内部 [offset, offset + buf.len) 区间内的字符，存放到 buf 中
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }

        let iter = BlockIter::new_multiblock(offset, offset + len, self.blk_size_log2());
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
                if self.blk_size_log2() > BLK_SIZE_LOG2_LIMIT {
                    return Err(SystemError::E2BIG);
                }

                let mut temp = Vec::new();
                temp.resize(1usize << self.blk_size_log2(), 0);
                BlockDevice::read_at(self, range.lba_start, 1, &mut temp[..])?;
                // 把数据从临时buffer复制到目标buffer
                buf_slice.copy_from_slice(&temp[range.begin..range.end]);
            }
        }
        return Ok(len);
    }

    /// 写入设备操作，把 buf 的数据写入到设备内部 [offset, offset + len) 区间内
    fn write_at(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        // assert!(len <= buf.len());
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }

        let iter = BlockIter::new_multiblock(offset, offset + len, self.blk_size_log2());
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
                if self.blk_size_log2() > BLK_SIZE_LOG2_LIMIT {
                    return Err(SystemError::E2BIG);
                }

                let mut temp = Vec::new();
                temp.resize(1usize << self.blk_size_log2(), 0);
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
    fn sync(&self) -> Result<(), SystemError> {
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
    #[allow(dead_code)]
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
        if __bytes_to_lba(self.begin, blk_size)
            != __bytes_to_lba(self.begin + blk_size - 1, blk_size)
            || lba_start == lba_end
        {
            return self.next_block();
        }

        let begin = self.begin % blk_size; // 因为是多个整块，这里必然是0
        let end = __lba_to_bytes(lba_end, blk_size) - self.begin;

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
    #[allow(dead_code)]
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
pub fn __bytes_to_lba(addr: usize, blk_size: usize) -> BlockId {
    return addr / blk_size;
}

/// 从lba id转换到字节地址， 返回lba_id的最左侧字节
#[inline]
pub fn __lba_to_bytes(lba_id: usize, blk_size: usize) -> BlockId {
    return lba_id * blk_size;
}
