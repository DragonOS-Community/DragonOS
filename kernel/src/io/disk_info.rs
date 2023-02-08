use super::device::BlockDevice;
use alloc::{string::String, sync::Arc, vec::Vec};

pub type SectorT = u64;

pub const BLK_TYPE_AHCI: u64 = 0;
pub const DISK_NAME_LEN: usize = 32; // 磁盘名称的最大长度
pub const BLK_GF_AHCI: u16 = 1 << 0; // 定义blk_gendisk中的标志位

/// @brief: 磁盘的分区信息 - (保留了c版本的数据信息)
#[derive(Debug)]
pub struct Partition {
    pub start_sector: SectorT, // 该分区的起始扇区
    pub bstart_lba: u64,       // 起始LBA号
    pub sectors_num: u64,      // 该分区的扇区数
    pub belong_disk: Arc<dyn BlockDevice>, // 当前分区所属的磁盘
    pub partno: u16, // 在磁盘上的分区号
    
    // struct block_device_request_queue *bd_queue; // 请求队列
    // struct vfs_superblock_t *bd_superblock;      // 执行超级块的指针
}

/// @brief: 磁盘信息 - (保留了c版本的数据信息)
pub struct DiskInfo {
    pub name: String,
    pub part_cnt: u16,
    pub flags: u16,
    pub part_s: Vec<Arc<Partition>>, // 磁盘分区数组
    
    // struct block_device_request_queue *request_queue; // 磁盘请求队列
    // mutex_lock: LockRef, // open()/close()操作的互斥锁
}

/// @brief: 分区信息 - 成员函数
impl Partition {
    pub fn new(
        start_sector: SectorT,
        bstart_lba: u64,
        sectors_num: u64,
        belong_disk: Arc<dyn BlockDevice>,
        partno: u16,
    ) -> Arc<Self> {
        return Arc::new(Partition {
            start_sector,
            bstart_lba,
            sectors_num,
            belong_disk,
            partno,
        });
    }
}

/// @brief: 磁盘信息 - 成员函数
impl DiskInfo {
    pub fn new(name: String, part_cnt: u16, flags: u16, part_s: Vec<Arc<Partition>>) -> Self {
        return DiskInfo {
            name,
            part_cnt,
            flags,
            part_s,
        };
    }
}

// 设计思路：
// 比如编写 ahci 时，你可以写一个 ahci_disk 类型
//
// struct ahci_disk {
//     blk_gendisk: disk_info,
//     private_data: any_types, // 这个就像ahci_private_data类型一样
// }
//
// impl BlockDeviceOp for ahci_disk {
//     read_at() {
//         // 这里你写ahci是怎么读取数据的
//     }
//     write_at() {
//         // 类似写怎么写数据, 最终调用的是你的这个函数
//     }
//     sync() {
//         // 写怎么把内存中的dirty数组存回去，或者先todo() -> 直接返回err_code
//     }
// }
//
// 因为rust没有struct的继承，所以只能使用组合的方式实现数据复用
