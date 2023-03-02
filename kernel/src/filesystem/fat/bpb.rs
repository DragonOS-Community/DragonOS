use alloc::sync::Arc;

use crate::io::{
    device::{BlockDevice, Device},
    disk_info::Partition,
};

/// 对于所有的FAT文件系统都适用的Bios Parameter Block结构体
#[derive(Debug, Clone, Copy, Default)]
pub struct BiosParameterBlock {
    /// 跳转指令
    pub jmp_boot: [u8; 3],

    /// 生产厂商名(表明是哪个操作系统格式化了这个卷)
    pub oem_name: [u8; 8],

    /// 每扇区字节数
    pub bytes_per_sector: u16,

    /// 每簇扇区数
    pub sector_per_cluster: u8,

    /// 保留扇区数
    pub rsvd_sec_cnt: u16,

    /// FAT表数量
    pub num_fats: u8,

    /// 根目录下的32字节目录项数量最大值（只对FAT12、FAT16生效）
    pub root_entries_cnt: u16,

    /// 当前分区的总扇区数（只对FAT12、FAT16生效）
    pub total_sectors_16: u16,

    /// 介质描述符
    pub media: u8,

    /// FAT12/16每FAT扇区数
    pub fat_size_16: u16,

    /// 每磁道扇区数
    pub sector_per_track: u16,

    /// 磁头数
    pub num_heads: u16,

    /// 隐藏扇区数
    pub hidden_sectors: u32,

    /// FAT32的总扇区数
    pub total_sectors_32: u32,

    /// FAT文件系统类型(以及他们的一些私有信息字段)
    pub fat_type: FATType,

    /// 引导扇区结束标志0xAA55
    pub trail_sig: u16,
}

#[derive(Debug, Clone, Copy)]
pub enum FATType {
    FAT12(BiosParameterBlockLegacy),
    FAT16(BiosParameterBlockLegacy),
    FAT32(BiosParameterBlockFAT32),
}

/// @brief FAT12/FAT16文件系统特有的BPB信息字段
#[derive(Debug, Clone, Copy, Default)]
pub struct BiosParameterBlockLegacy {
    /// int0x13的驱动器号
    pub drive_num: u8,
    /// 保留字段
    pub reserved1: u8,
    /// 扩展引导标记
    pub boot_sig: u8,
    /// 卷号
    /// BS_VolID
    pub volume_id: u32,
    /// 文件系统类型
    pub filesystem_type: u32,
}

/// @brief FAT32文件系统特有的BPB信息字段
#[derive(Debug, Clone, Copy, Default)]
pub struct BiosParameterBlockFAT32 {
    /// FAT32每FAT扇区数
    /// BPB_FATSz32
    pub fat_size_32: u32,

    /// 扩展标记
    /// Bits 0-3 -- Zero based number of active FAT（活跃的FAT表的编号）
    /// Only valid if mirroring iFAT32s disabled
    /// Bits 4-6 -- 保留
    /// Bit 7 -- 0表示在运行时，所有的FAT表都互为镜像
    ///       -- 1表示只使用1个FAT表，具体使用的FAT表的编号需要看Bits 0-3
    /// Bits 8-15 -- 保留备用
    /// BPB_ExtFlags
    pub ext_flags: u16,

    /// 文件系统版本号。
    /// 高字节表示主版本号，低字节表示次版本号。
    /// BPB_FSVer
    pub fs_version: u16,

    /// 根目录的簇号
    /// BPB_RootClus
    pub root_cluster: u32,

    /// FsInfo结构体所在的扇区号
    pub fs_info: u16,

    /// 如果这个值非0,那么它表示备份的引导扇区号。
    /// BPB_BkBootSec
    pub backup_boot_sec: u16,

    /// 保留备用
    /// BPB_Reserved0
    pub reserved0: [u8; 12],

    /// int0x13的驱动器号
    /// BS_DrvNum
    pub drive_num: u8,

    pub reserved1: u8,

    /// 引导标记
    /// BS_BootSig
    pub boot_sig: u8,

    /// 卷号
    /// BS_VolID
    pub volume_id: u32,

    /// 卷标
    /// BS_VolLab
    pub volume_label: [u8; 11],

    /// 文件系统类型
    /// BS_FilSystype
    pub filesystem_type: [u8; 8],
}

impl Default for FATType {
    fn default() -> Self {
        return FATType::FAT32(BiosParameterBlockFAT32::default());
    }
}

impl BiosParameterBlock {
    pub fn new(partition: Arc<Partition>) -> Result<BiosParameterBlock, i32> {
        let mut bpb = BiosParameterBlock::default();
        // let device: Arc<dyn Device> = partition.belong_disk.device();

        todo!()
    }

    /// @brief 验证BPB信息是否合法
    pub fn validate(&self, bpb32: &BiosParameterBlockFAT32) -> Result<(), i32> {
        todo!()
    }

    /// @brief 判断当前是否为fat32的bpb
    fn is_fat32(&self) -> bool {
        // fat32的bpb，这个字段是0
        return self.total_sectors_16 == 0;
    }

    pub fn get_volume_id(&self) -> u32 {
        match self.fat_type {
            FATType::FAT12(f) | FATType::FAT16(f) => {
                return f.volume_id;
            }

            FATType::FAT32(f) => {
                return f.volume_id;
            }
        }
    }
}
