#![allow(dead_code)]
use super::device::BlockDevice;

use alloc::sync::{Arc, Weak};

pub type SectorT = u64;

pub const BLK_TYPE_AHCI: u64 = 0;
pub const DISK_NAME_LEN: usize = 32; // 磁盘名称的最大长度
pub const BLK_GF_AHCI: u16 = 1 << 0; // 定义blk_gendisk中的标志位

/// @brief: 磁盘的分区信息 - (保留了c版本的数据信息)
#[derive(Debug)]
pub struct Partition {
    pub start_sector: SectorT,   // 该分区的起始扇区
    pub lba_start: u64,          // 起始LBA号
    pub sectors_num: u64,        // 该分区的扇区数
    disk: Weak<dyn BlockDevice>, // 当前分区所属的磁盘
    pub partno: u16,             // 在磁盘上的分区号

                                 // struct block_device_request_queue *bd_queue; // 请求队列
                                 // struct vfs_superblock_t *bd_superblock;      // 执行超级块的指针
}

/// @brief: 分区信息 - 成员函数
impl Partition {
    /// @brief: 为 disk new 一个分区结构体
    pub fn new(
        start_sector: SectorT,
        lba_start: u64,
        sectors_num: u64,
        disk: Weak<dyn BlockDevice>,
        partno: u16,
    ) -> Arc<Self> {
        return Arc::new(Partition {
            start_sector,
            lba_start,
            sectors_num,
            disk,
            partno,
        });
    }

    /// @brief 获取当前分区所属的磁盘的Arc指针
    #[inline]
    pub fn disk(&self) -> Arc<dyn BlockDevice> {
        return self.disk.upgrade().unwrap();
    }
}
