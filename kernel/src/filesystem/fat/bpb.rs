#![allow(dead_code)]
use alloc::sync::Arc;
use log::error;
use system_error::SystemError;

use crate::{
    driver::base::block::{block_device::LBA_SIZE, gendisk::GenDisk, SeekFrom},
    libs::vec_cursor::VecCursor,
};

use super::fs::{Cluster, FATFileSystem};

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

    /// FsInfo结构体在分区内的偏移量（单位：扇区）
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

impl FATType {
    /// @brief 获取指定的簇对应的FAT表项在分区内的字节偏移量
    ///
    /// @param cluster 要查询的簇
    /// @param fat_start_sector FAT表的起始扇区
    /// @param bytes_per_sec 文件系统每扇区的字节数
    ///
    /// @return 指定的簇对应的FAT表项在分区内的字节偏移量
    #[inline]
    pub fn get_fat_bytes_offset(
        &self,
        cluster: Cluster,
        fat_start_sector: u64,
        bytes_per_sec: u64,
    ) -> u64 {
        let current_cluster = cluster.cluster_num;
        // 要查询的簇，在FAT表中的字节偏移量
        let fat_bytes_offset = match self {
            FATType::FAT12(_) => current_cluster + (current_cluster / 2),
            FATType::FAT16(_) => current_cluster * 2,
            FATType::FAT32(_) => current_cluster * 4,
        };
        let fat_sec_number = fat_start_sector + (fat_bytes_offset / bytes_per_sec);
        let fat_ent_offset = fat_bytes_offset % bytes_per_sec;
        return fat_sec_number * bytes_per_sec + fat_ent_offset;
    }
}

impl BiosParameterBlockLegacy {
    /// @brief 验证FAT12/16 BPB的信息是否合法
    fn validate(&self, _bpb: &BiosParameterBlock) -> Result<(), SystemError> {
        return Ok(());
    }
}

impl BiosParameterBlockFAT32 {
    /// @brief 验证BPB32的信息是否合法
    fn validate(&self, bpb: &BiosParameterBlock) -> Result<(), SystemError> {
        if bpb.fat_size_16 != 0 {
            error!("Invalid fat_size_16 value in BPB (should be zero for FAT32)");
            return Err(SystemError::EINVAL);
        }

        if bpb.root_entries_cnt != 0 {
            error!("Invalid root_entries value in BPB (should be zero for FAT32)");
            return Err(SystemError::EINVAL);
        }

        if bpb.total_sectors_16 != 0 {
            error!("Invalid total_sectors_16 value in BPB (should be zero for FAT32)");
            return Err(SystemError::EINVAL);
        }

        if self.fat_size_32 == 0 {
            error!("Invalid fat_size_32 value in BPB (should be non-zero for FAT32)");
            return Err(SystemError::EINVAL);
        }

        if self.fs_version != 0 {
            error!("Unknown FAT FS version");
            return Err(SystemError::EINVAL);
        }

        return Ok(());
    }
}

impl BiosParameterBlock {
    pub fn new(gendisk: &Arc<GenDisk>) -> Result<BiosParameterBlock, SystemError> {
        let mut v = vec![0; LBA_SIZE];
        // 读取分区的引导扇区
        gendisk.read_at(&mut v, 0)?;
        // 获取指针对象
        let mut cursor = VecCursor::new(v);

        let mut bpb = BiosParameterBlock::default();

        cursor.read_exact(&mut bpb.jmp_boot)?;
        cursor.read_exact(&mut bpb.oem_name)?;
        bpb.bytes_per_sector = cursor.read_u16()?;
        bpb.sector_per_cluster = cursor.read_u8()?;
        bpb.rsvd_sec_cnt = cursor.read_u16()?;
        bpb.num_fats = cursor.read_u8()?;
        bpb.root_entries_cnt = cursor.read_u16()?;
        bpb.total_sectors_16 = cursor.read_u16()?;
        bpb.media = cursor.read_u8()?;
        bpb.fat_size_16 = cursor.read_u16()?;
        bpb.sector_per_track = cursor.read_u16()?;
        bpb.num_heads = cursor.read_u16()?;
        bpb.hidden_sectors = cursor.read_u32()?;
        bpb.total_sectors_32 = cursor.read_u32()?;

        let mut bpb32 = BiosParameterBlockFAT32 {
            fat_size_32: cursor.read_u32()?,
            ext_flags: cursor.read_u16()?,
            fs_version: cursor.read_u16()?,
            root_cluster: cursor.read_u32()?,
            fs_info: cursor.read_u16()?,
            backup_boot_sec: cursor.read_u16()?,
            drive_num: cursor.read_u8()?,
            reserved1: cursor.read_u8()?,
            boot_sig: cursor.read_u8()?,
            volume_id: cursor.read_u32()?,
            ..Default::default()
        };
        cursor.read_exact(&mut bpb32.reserved0)?;
        cursor.read_exact(&mut bpb32.volume_label)?;
        cursor.read_exact(&mut bpb32.filesystem_type)?;

        // 跳过启动代码
        cursor.seek(SeekFrom::SeekCurrent(420))?;
        // 读取尾部的启动扇区标志
        bpb.trail_sig = cursor.read_u16()?;

        // 计算根目录项占用的空间（单位：字节）
        let root_sectors = (bpb.root_entries_cnt as u32 * 32).div_ceil(bpb.bytes_per_sector as u32);

        // 每FAT扇区数
        let fat_size = if bpb.fat_size_16 != 0 {
            bpb.fat_size_16 as u32
        } else {
            bpb32.fat_size_32
        };

        // 当前分区总扇区数
        let total_sectors = if bpb.total_sectors_16 != 0 {
            bpb.total_sectors_16 as u32
        } else {
            bpb.total_sectors_32
        };

        // 数据区扇区数
        let data_sectors = total_sectors
            - ((bpb.rsvd_sec_cnt as u32) + (bpb.num_fats as u32) * fat_size + root_sectors);
        // 总的数据簇数量（向下对齐）
        let count_clusters = data_sectors / (bpb.sector_per_cluster as u32);

        // 设置FAT类型
        bpb.fat_type = if count_clusters < FATFileSystem::FAT12_MAX_CLUSTER {
            FATType::FAT12(BiosParameterBlockLegacy::default())
        } else if count_clusters <= FATFileSystem::FAT16_MAX_CLUSTER {
            FATType::FAT16(BiosParameterBlockLegacy::default())
        } else if count_clusters < FATFileSystem::FAT32_MAX_CLUSTER {
            FATType::FAT32(bpb32)
        } else {
            // 都不符合条件，报错
            return Err(SystemError::EINVAL);
        };

        // 验证BPB的信息是否合法
        bpb.validate()?;

        return Ok(bpb);
    }

    /// @brief 验证BPB的信息是否合法
    pub fn validate(&self) -> Result<(), SystemError> {
        // 校验每扇区字节数是否合法
        if self.bytes_per_sector.count_ones() != 1 {
            error!("Invalid bytes per sector(not a power of 2)");
            return Err(SystemError::EINVAL);
        } else if self.bytes_per_sector < 512 {
            error!("Invalid bytes per sector (value < 512)");
            return Err(SystemError::EINVAL);
        } else if self.bytes_per_sector > 4096 {
            error!("Invalid bytes per sector (value > 4096)");
            return Err(SystemError::EINVAL);
        }

        if self.rsvd_sec_cnt < 1 {
            error!("Invalid rsvd_sec_cnt value in BPB");
            return Err(SystemError::EINVAL);
        }

        if self.num_fats == 0 {
            error!("Invalid fats value in BPB");
            return Err(SystemError::EINVAL);
        }

        if (self.total_sectors_16 == 0) && (self.total_sectors_32 == 0) {
            error!("Invalid BPB (total_sectors_16 or total_sectors_32 should be non-zero)");
            return Err(SystemError::EINVAL);
        }

        let fat_size = match self.fat_type {
            FATType::FAT32(bpb32) => {
                bpb32.validate(self)?;
                bpb32.fat_size_32
            }
            FATType::FAT16(bpb_legacy) | FATType::FAT12(bpb_legacy) => {
                bpb_legacy.validate(self)?;
                self.fat_size_16 as u32
            }
        };

        let root_sectors =
            (self.root_entries_cnt as u32 * 32).div_ceil(self.bytes_per_sector as u32);

        // 当前分区总扇区数
        let total_sectors = if self.total_sectors_16 != 0 {
            self.total_sectors_16 as u32
        } else {
            self.total_sectors_32
        };

        let first_data_sector =
            (self.rsvd_sec_cnt as u32) + (self.num_fats as u32) * fat_size + root_sectors;

        // 总扇区数应当大于第一个数据扇区的扇区号
        if total_sectors <= first_data_sector {
            error!("Total sectors lesser than first data sector");
            return Err(SystemError::EINVAL);
        }

        return Ok(());
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
