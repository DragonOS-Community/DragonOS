#![allow(dead_code)]
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use super::block_device::{BlockDevice, GeneralBlockRange};

pub type SectorT = u64;

/// @brief: 磁盘的分区信息 - (保留了c版本的数据信息)
#[derive(Debug)]
pub struct Partition {
    pub start_sector: SectorT,           // 该分区的起始扇区
    pub lba_start: u64,                  // 起始LBA号
    pub sectors_num: u64,                // 该分区的扇区数
    disk: Option<Weak<dyn BlockDevice>>, // 当前分区所属的磁盘
    pub partno: u16,                     // 在磁盘上的分区号
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
            disk: Some(disk),
            partno,
        });
    }

    pub fn new_raw(start_sector: SectorT, lba_start: u64, sectors_num: u64, partno: u16) -> Self {
        return Partition {
            start_sector,
            lba_start,
            sectors_num,
            disk: None,
            partno,
        };
    }

    /// @brief 获取当前分区所属的磁盘的Arc指针
    #[inline]
    pub fn disk(&self) -> Arc<dyn BlockDevice> {
        return self.disk.as_ref().unwrap().upgrade().unwrap();
    }
}

impl TryInto<GeneralBlockRange> for Partition {
    type Error = SystemError;

    fn try_into(self) -> Result<GeneralBlockRange, Self::Error> {
        let start = usize::try_from(self.lba_start).map_err(|_| SystemError::EINVAL)?;
        let end = self
            .lba_start
            .checked_add(self.sectors_num)
            .and_then(|end| usize::try_from(end).ok())
            .ok_or(SystemError::EINVAL)?;
        if let Some(range) = GeneralBlockRange::new(start, end) {
            return Ok(range);
        } else {
            return Err(SystemError::EINVAL);
        }
    }
}
