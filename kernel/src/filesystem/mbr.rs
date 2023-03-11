#![allow(dead_code)]
use core::default::Default;

/// @brief MBR硬盘分区表项的结构
#[repr(packed)]
#[derive(Debug, Clone, Copy)]
pub struct MbrDiskPartitionTableEntry {
    pub flags: u8,                     // 引导标志符，标记此分区为活动分区
    pub starting_head: u8,             // 起始磁头号
    pub starting_sector_cylinder: u16, // sector : 低6, cylinder : 高10;   起始扇区号 + 起始柱面号
    pub part_type: u8,                 // 分区类型ID
    pub ending_head: u8,               // 结束磁头号
    pub ending_sector_cylingder: u16, // ending_sector : 低6, ending_cylinder : 高10;  结束扇区号 + 结束柱面号
    pub starting_lba: u32,            // 起始逻辑扇区
    pub total_sectors: u32,           // 分区占用的磁盘扇区数
}

impl MbrDiskPartitionTableEntry {
    pub fn starting_sector(&self) -> u16 {
        return self.starting_sector_cylinder & ((1 << 6) - 1) as u16;
    }
    pub fn starting_cylinder(&self) -> u16 {
        return (self.starting_sector_cylinder >> 6) & ((1 << 10) - 1) as u16;
    }
    pub fn ending_sector(&self) -> u16 {
        return self.ending_sector_cylingder & ((1 << 6) - 1) as u16;
    }
    pub fn ending_cylinder(&self) -> u16 {
        return (self.ending_sector_cylingder >> 6) & ((1 << 10) - 1) as u16;
    }
}

/// @brief MBR磁盘分区表结构体
#[repr(packed)]
#[derive(Debug, Clone, Copy)]
pub struct MbrDiskPartionTable {
    pub reserved: [u8; 446],
    pub dpte: [MbrDiskPartitionTableEntry; 4], // 磁盘分区表项
    pub bs_trailsig: u16,
}

impl Default for MbrDiskPartitionTableEntry {
    fn default() -> Self {
        MbrDiskPartitionTableEntry {
            flags: 0,
            starting_head: 0,
            starting_sector_cylinder: 0,
            part_type: 0,
            ending_head: 0,
            ending_sector_cylingder: 0,
            starting_lba: 0,
            total_sectors: 0,
        }
    }
}

impl Default for MbrDiskPartionTable {
    fn default() -> Self {
        MbrDiskPartionTable {
            reserved: [0; 446],
            dpte: [Default::default(); 4],
            bs_trailsig: Default::default(),
        }
    }
}
