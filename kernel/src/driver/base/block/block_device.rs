/// 引入Module
use crate::driver::{
    base::{
        device::{
            device_number::{DeviceNumber, Major},
            DevName, Device, DeviceError, IdTable, BLOCKDEVS,
        },
        map::{
            DeviceStruct, DEV_MAJOR_DYN_END, DEV_MAJOR_DYN_EXT_END, DEV_MAJOR_DYN_EXT_START,
            DEV_MAJOR_HASH_SIZE, DEV_MAJOR_MAX,
        },
    },
    block::cache::{cached_block_device::BlockCache, BlockCacheError, BLOCK_SIZE},
};

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;
use log::error;
use system_error::SystemError;

use super::{disk_info::Partition, gendisk::GenDisk, manager::BlockDevMeta};

// 该文件定义了 Device 和 BlockDevice 的接口
// Notice 设备错误码使用 Posix 规定的 int32_t 的错误码表示，而不是自己定义错误enum

// 使用方法:
// 假设 blk_dev 是块设备
// <blk_dev as Device>::read_at() 调用的是Device的函数
// <blk_dev as BlockDevice>::read_at() 调用的是BlockDevice的函数

/// 定义类型
pub type BlockId = usize;

/// 定义常量
pub const BLK_SIZE_LOG2_LIMIT: u8 = 12; // 设定块设备的块大小不能超过 1 << 12.
/// 在DragonOS中，我们认为磁盘的每个LBA大小均为512字节。（注意，文件系统的1个扇区可能事实上是多个LBA）
pub const LBA_SIZE: usize = 512;

#[derive(Debug, Clone, Copy)]
pub struct GeneralBlockRange {
    pub lba_start: usize,
    pub lba_end: usize,
}

impl GeneralBlockRange {
    pub fn new(lba_start: usize, lba_end: usize) -> Option<Self> {
        if lba_start >= lba_end {
            return None;
        }
        return Some(GeneralBlockRange { lba_start, lba_end });
    }

    #[inline]
    pub fn len(&self) -> usize {
        return self.lba_end - self.lba_start;
    }

    /// 取交集
    pub fn intersects_with(&self, rhs: &Self) -> Option<Self> {
        // 检查是否相交
        if self.lba_start <= rhs.lba_end && self.lba_end >= rhs.lba_start {
            // 计算相交部分的起始和结束 LBA
            let start = usize::max(self.lba_start, rhs.lba_start);
            let end = usize::min(self.lba_end, rhs.lba_end);
            // 返回相交部分
            GeneralBlockRange::new(start, end)
        } else {
            // 不相交，返回 None
            None
        }
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
            blk_size_log2,
            multiblock: false,
        };
    }
    pub fn new_multiblock(start_addr: usize, end_addr: usize, blk_size_log2: u8) -> BlockIter {
        return BlockIter {
            begin: start_addr,
            end: end_addr,

            blk_size_log2,
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
            begin,
            end,
            blk_size_log2,
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
            lba_start,
            lba_end,
            begin,
            end,
            blk_size_log2,
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

/// @brief 块设备应该实现的操作
pub trait BlockDevice: Device {
    /// # dev_name
    /// 返回块设备的名字
    fn dev_name(&self) -> &DevName;

    fn blkdev_meta(&self) -> &BlockDevMeta;

    /// 获取设备的扇区范围
    fn disk_range(&self) -> GeneralBlockRange;

    /// @brief: 在块设备中，从第lba_id_start个块开始，读取count个块数据，存放到buf中
    ///
    /// @parameter lba_id_start: 起始块
    /// @parameter count: 读取块的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回 Ok(操作的长度) 其中单位是字节；
    ///          否则返回Err(错误码)，其中错误码为负数；
    ///          如果操作异常，但是并没有检查出什么错误，将返回Err(已操作的长度)
    fn read_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError>;

    /// @brief: 在块设备中，从第lba_id_start个块开始，把buf中的count个块数据，存放到设备中
    /// @parameter lba_id_start: 起始块
    /// @parameter count: 写入块的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回 Ok(操作的长度) 其中单位是字节；
    ///          否则返回Err(错误码)，其中错误码为负数；
    ///          如果操作异常，但是并没有检查出什么错误，将返回Err(已操作的长度)
    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError>;

    /// @brief: 同步磁盘信息，把所有的dirty数据写回硬盘 - 待实现
    fn sync(&self) -> Result<(), SystemError>;

    /// @brief: 每个块设备都必须固定自己块大小，而且该块大小必须是2的幂次
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

    /// # 函数的功能
    /// 经由Cache对块设备的读操作
    fn read_at(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.cache_read(lba_id_start, count, buf)
    }

    /// # 函数的功能
    ///  经由Cache对块设备的写操作
    fn write_at(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        self.cache_write(lba_id_start, count, buf)
    }

    /// # 函数的功能
    /// 其功能对外而言和read_at函数完全一致，但是加入blockcache的功能
    fn cache_read(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let cache_response = BlockCache::read(lba_id_start, count, buf);
        if let Err(e) = cache_response {
            match e {
                BlockCacheError::StaticParameterError => {
                    BlockCache::init();
                    let ans = self.read_at_sync(lba_id_start, count, buf)?;
                    return Ok(ans);
                }
                BlockCacheError::BlockFaultError(fail_vec) => {
                    let ans = self.read_at_sync(lba_id_start, count, buf)?;
                    let _ = BlockCache::insert(fail_vec, buf);
                    return Ok(ans);
                }
                _ => {
                    let ans = self.read_at_sync(lba_id_start, count, buf)?;
                    return Ok(ans);
                }
            }
        } else {
            return Ok(count * BLOCK_SIZE);
        }
    }

    /// # 函数功能
    /// 其功能对外而言和write_at函数完全一致，但是加入blockcache的功能
    fn cache_write(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let _cache_response = BlockCache::immediate_write(lba_id_start, count, buf);
        self.write_at_sync(lba_id_start, count, buf)
    }

    fn write_at_bytes(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }

        let iter = BlockIter::new_multiblock(offset, offset + len, self.blk_size_log2());
        let multi = iter.multiblock;

        for range in iter {
            let buf_begin = range.origin_begin() - offset; // 本次读操作的起始位置/已经读了这么多字节
            let buf_end = range.origin_end() - offset;
            let buf_slice = &buf[buf_begin..buf_end];
            let count: usize = range.lba_end - range.lba_start;
            let full = multi && range.is_multi() || !multi && range.is_full();

            if full {
                self.write_at(range.lba_start, count, buf_slice)?;
            } else {
                if self.blk_size_log2() > BLK_SIZE_LOG2_LIMIT {
                    return Err(SystemError::E2BIG);
                }

                let mut temp = vec![0; 1usize << self.blk_size_log2()];
                // 由于块设备每次读写都是整块的，在不完整写入之前，必须把不完整的地方补全
                self.read_at(range.lba_start, 1, &mut temp[..])?;
                // 把数据从临时buffer复制到目标buffer
                temp[range.begin..range.end].copy_from_slice(buf_slice);
                self.write_at(range.lba_start, 1, &temp[..])?;
            }
        }
        return Ok(len);
    }

    fn read_at_bytes(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
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
            let count: usize = range.lba_end - range.lba_start;
            let full = multi && range.is_multi() || !multi && range.is_full();

            // 读取整个block作为有效数据
            if full {
                // 调用 BlockDevice::read_at() 直接把引用传进去，不是把整个数组move进去
                self.read_at(range.lba_start, count, buf_slice)?;
            } else {
                // 判断块的长度不能超过最大值
                if self.blk_size_log2() > BLK_SIZE_LOG2_LIMIT {
                    return Err(SystemError::E2BIG);
                }

                let mut temp = vec![0; 1usize << self.blk_size_log2()];
                self.read_at(range.lba_start, 1, &mut temp[..])?;

                // 把数据从临时buffer复制到目标buffer
                buf_slice.copy_from_slice(&temp[range.begin..range.end]);
            }
        }
        return Ok(len);
    }

    /// # gendisk注册成功的回调函数
    fn callback_gendisk_registered(&self, _gendisk: &Arc<GenDisk>) -> Result<(), SystemError> {
        Ok(())
    }
}

/// @brief 块设备框架函数集
pub struct BlockDeviceOps;

impl BlockDeviceOps {
    /// @brief: 主设备号转下标
    /// @parameter: major: 主设备号
    /// @return: 返回下标
    #[allow(dead_code)]
    fn major_to_index(major: Major) -> usize {
        return (major.data() % DEV_MAJOR_HASH_SIZE) as usize;
    }

    /// @brief: 动态获取主设备号
    /// @parameter: None
    /// @return: 如果成功，返回主设备号，否则，返回错误码
    #[allow(dead_code)]
    fn find_dynamic_major() -> Result<Major, SystemError> {
        let blockdevs = BLOCKDEVS.lock();
        // 寻找主设备号为234～255的设备
        for index in ((DEV_MAJOR_DYN_END.data())..DEV_MAJOR_HASH_SIZE).rev() {
            if let Some(item) = blockdevs.get(index as usize) {
                if item.is_empty() {
                    return Ok(Major::new(index)); // 返回可用的主设备号
                }
            }
        }
        // 寻找主设备号在384～511的设备
        for index in
            ((DEV_MAJOR_DYN_EXT_END.data() + 1)..(DEV_MAJOR_DYN_EXT_START.data() + 1)).rev()
        {
            if let Some(blockdevss) = blockdevs.get(Self::major_to_index(Major::new(index))) {
                let mut flag = true;
                for item in blockdevss {
                    if item.device_number().major() == Major::new(index) {
                        flag = false;
                        break;
                    }
                }
                if flag {
                    // 如果数组中不存在主设备号等于index的设备
                    return Ok(Major::new(index)); // 返回可用的主设备号
                }
            }
        }
        return Err(SystemError::EBUSY);
    }

    /// @brief: 注册设备号，该函数需要指定主设备号
    /// @parameter: from: 主设备号
    ///             count: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回设备号，否则，返回错误码
    #[allow(dead_code)]
    pub fn register_blockdev_region(
        from: DeviceNumber,
        count: u32,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        Self::__register_blockdev_region(from, count, name)
    }

    /// @brief: 注册设备号，该函数自动分配主设备号
    /// @parameter: baseminor: 主设备号
    ///             count: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回，否则，返回false
    #[allow(dead_code)]
    pub fn alloc_blockdev_region(
        baseminor: u32,
        count: u32,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        Self::__register_blockdev_region(
            DeviceNumber::new(Major::UNNAMED_MAJOR, baseminor),
            count,
            name,
        )
    }

    /// @brief: 注册设备号
    /// @parameter: device_number: 设备号，主设备号如果为0，则动态分配
    ///             minorct: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回设备号，否则，返回错误码
    fn __register_blockdev_region(
        device_number: DeviceNumber,
        minorct: u32,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        let mut major = device_number.major();
        let baseminor = device_number.minor();
        if major >= DEV_MAJOR_MAX {
            error!(
                "DEV {} major requested {:?} is greater than the maximum {}\n",
                name,
                major,
                DEV_MAJOR_MAX.data() - 1
            );
        }
        if minorct > DeviceNumber::MINOR_MASK + 1 - baseminor {
            error!("DEV {} minor range requested ({}-{}) is out of range of maximum range ({}-{}) for a single major\n",
                name, baseminor, baseminor + minorct - 1, 0, DeviceNumber::MINOR_MASK);
        }
        let blockdev = DeviceStruct::new(DeviceNumber::new(major, baseminor), minorct, name);
        if major == Major::UNNAMED_MAJOR {
            // 如果主设备号为0,则自动分配主设备号
            major = Self::find_dynamic_major().expect("Find synamic major error.\n");
        }
        if let Some(items) = BLOCKDEVS.lock().get_mut(Self::major_to_index(major)) {
            let mut insert_index: usize = 0;
            for (index, item) in items.iter().enumerate() {
                insert_index = index;
                match item.device_number().major().cmp(&major) {
                    core::cmp::Ordering::Less => continue,
                    core::cmp::Ordering::Greater => {
                        break; // 大于则向后插入
                    }
                    core::cmp::Ordering::Equal => {
                        if item.device_number().minor() + item.minorct() <= baseminor {
                            continue; // 下一个主设备号大于或者次设备号大于被插入的次设备号最大值
                        }
                        if item.base_minor() >= baseminor + minorct {
                            break; // 在此处插入
                        }
                        return Err(SystemError::EBUSY); // 存在重合的次设备号
                    }
                }
            }
            items.insert(insert_index, blockdev);
        }

        return Ok(DeviceNumber::new(major, baseminor));
    }

    /// @brief: 注销设备号
    /// @parameter: major: 主设备号，如果为0，动态分配
    ///             baseminor: 起始次设备号
    ///             minorct: 次设备号数量
    /// @return: 如果注销成功，返回()，否则，返回错误码
    fn __unregister_blockdev_region(
        device_number: DeviceNumber,
        minorct: u32,
    ) -> Result<(), SystemError> {
        if let Some(items) = BLOCKDEVS
            .lock()
            .get_mut(Self::major_to_index(device_number.major()))
        {
            for (index, item) in items.iter().enumerate() {
                if item.device_number() == device_number && item.minorct() == minorct {
                    // 设备号和数量都相等
                    items.remove(index);
                    return Ok(());
                }
            }
        }
        return Err(SystemError::EBUSY);
    }

    /// @brief: 块设备注册
    /// @parameter: cdev: 字符设备实例
    ///             dev_t: 字符设备号
    ///             range: 次设备号范围
    /// @return: none
    #[allow(dead_code)]
    pub fn bdev_add(_bdev: Arc<dyn BlockDevice>, id_table: IdTable) -> Result<(), DeviceError> {
        if id_table.device_number().data() == 0 {
            error!("Device number can't be 0!\n");
        }
        todo!("bdev_add")
        // return device_manager().add_device(bdev.id_table(), bdev.device());
    }

    /// @brief: block设备注销
    /// @parameter: dev_t: 字符设备号
    ///             range: 次设备号范围
    /// @return: none
    #[allow(dead_code)]
    pub fn bdev_del(_devnum: DeviceNumber, _range: usize) {
        unimplemented!();
    }
}
