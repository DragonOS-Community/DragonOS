use super::libs::lockref::LockRef;
use device::BlockDeviceOp;
use alloc::{string::String, sync::Arc, vec::Vec};

pub type sector_t = u64;

pub const BLK_TYPE_AHCI: u64 = 0;
pub const DISK_NAME_LEN: usize = 32; // 磁盘名称的最大长度
pub const BLK_GF_AHCI: u16 = (1 << 0); // 定义blk_gendisk中的标志位

/// @brief: 磁盘的分区信息 - (保留了c版本的数据信息)
pub struct partition_info {
    start_sector: sector_t, // 该分区的起始扇区
    bstart_LBA: u64,        // 起始LBA号
    sectors_num: u64,       // 该分区的扇区数
    // struct vfs_superblock_t *bd_superblock;      // 执行超级块的指针
    belong_disk: Arc<disk_info>, // 当前分区所属的磁盘
    // struct block_device_request_queue *bd_queue; // 请求队列
    partno: u16, // 在磁盘上的分区号
}

/// @brief: 磁盘信息 - (保留了c版本的数据信息)
pub struct disk_info {
    name: String,
    part_cnt: u16,
    flags: u16,
    part_s: Vec<partition>, // 磁盘分区数组
    // struct block_device_request_queue *request_queue; // 磁盘请求队列
    mutex_lock: LockRef, // open()/close()操作的互斥锁
}


/// @brief: 分区信息 - 成员函数
impl partition_info {
    pub fn new(
        start_sector: sector_t, 
        bstart_LBA: u64,       
        sectors_num: u64,    
        belong_disk: Arc<disk_info>, 
        partno: u16, 
    ) -> Self {
        partition_info {
            start_sector, 
            bstart_LBA,       
            sectors_num,     
            belong_disk,
            partno: u16, 
        }
    }
}

/// @brief: 磁盘信息 - 成员函数
impl disk_info {
    pub fn new(
        name: String,
        part_cnt: u16,
        flags: u16,
        part_s: Vec<partition>,
    ) -> Self {
        disk_info {
            name,
            part_cnt,
            flags,
            part_s,
            mutex_lock: LockRef::new(),
        }
    }
}

/// 设计思路：
/// 比如编写ahci时，你可以写一个 ahci_disk 类型
/// 
/// struct ahci_disk {
///     blk_gendisk: disk_info,
///     private_data: any_types, // 这个就像ahci_private_data类型一样
/// }
/// 
/// impl BlockDeviceOp for ahci_disk {
///     read_at() {
///         // 这里你写ahci是怎么读取数据的
///     }
///     write_at() {
///         // 类似写怎么写数据, 最终调用的是你的这个函数    
///     }  
///     sync() {
///         // 写怎么把内存中的dirty数组存回去，或者先todo() -> 直接返回err_code
///     }
/// }
/// 
/// 因为rust没有struct的继承，所以只能使用组合的方式实现数据复用
