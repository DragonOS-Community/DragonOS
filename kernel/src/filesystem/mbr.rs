use core::{default::Default, mem::size_of};

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use log::debug;
use system_error::SystemError;

use crate::{
    driver::base::block::{block_device::BlockDevice, disk_info::Partition, SeekFrom},
    libs::vec_cursor::VecCursor,
};

/// @brief MBR硬盘分区表项的结构
#[repr(packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MbrDiskPartitionTableEntry {
    pub flags: u8,                     // 引导标志符，标记此分区为活动分区
    pub starting_head: u8,             // 起始磁头号
    pub starting_sector_cylinder: u16, // sector : 低6, cylinder : 高10;   起始扇区号 + 起始柱面号
    pub part_type: u8,                 // 分区类型ID
    pub ending_head: u8,               // 结束磁头号
    pub ending_sector_cylinder: u16, // ending_sector : 低6, ending_cylinder : 高10;  结束扇区号 + 结束柱面号
    pub starting_lba: u32,           // 起始逻辑扇区
    pub total_sectors: u32,          // 分区占用的磁盘扇区数
}

impl MbrDiskPartitionTableEntry {
    pub fn starting_sector(&self) -> u32 {
        return (self.starting_sector_cylinder & ((1 << 6) - 1)).into();
    }
    pub fn starting_cylinder(&self) -> u16 {
        return (self.starting_sector_cylinder >> 6) & ((1 << 10) - 1) as u16;
    }
    pub fn ending_sector(&self) -> u32 {
        self.starting_sector() + self.total_sectors - 1
    }

    pub fn ending_cylinder(&self) -> u16 {
        return (self.ending_sector_cylinder >> 6) & ((1 << 10) - 1) as u16;
    }

    pub fn is_valid(&self) -> bool {
        // 其他更多的可能判断条件
        self.starting_sector() <= self.ending_sector()
            && self.starting_cylinder() <= self.ending_cylinder()
            && self.starting_lba != 0
            && self.total_sectors != 0
            && self.part_type != 0
    }
}

/// @brief MBR磁盘分区表结构体
#[repr(packed)]
#[derive(Debug, Clone, Copy)]
pub struct MbrDiskPartionTable {
    pub _reserved: [u8; 446],
    pub dpte: [MbrDiskPartitionTableEntry; 4], // 磁盘分区表项
    pub bs_trailsig: u16,
}

impl Default for MbrDiskPartionTable {
    fn default() -> Self {
        MbrDiskPartionTable {
            _reserved: [0; 446],
            dpte: [Default::default(); 4],
            bs_trailsig: Default::default(),
        }
    }
}

impl MbrDiskPartionTable {
    /// # 从磁盘读取MBR分区表 - 从磁盘设备中读取并解析MBR分区表
    ///
    /// 这个函数从提供的磁盘设备中读取MBR分区表，并将其解析为一个`MbrDiskPartionTable`实例。
    ///
    /// ## 参数
    ///
    /// - `disk`: Arc<dyn BlockDevice> - 一个磁盘设备的共享引用，用于从磁盘读取数据。
    ///
    /// ## 返回值
    ///
    /// - `Ok(MbrDiskPartionTable)`: 成功解析的分区表实例。
    /// - `Err(SystemError)`: 读取磁盘失败或其他系统错误。
    pub fn from_disk(disk: Arc<dyn BlockDevice>) -> Result<MbrDiskPartionTable, SystemError> {
        let mut table: MbrDiskPartionTable = Default::default();

        // 数据缓冲区
        let mut buf: Vec<u8> = vec![0; size_of::<MbrDiskPartionTable>()];
        buf.resize(size_of::<MbrDiskPartionTable>(), 0);

        disk.read_at_sync(0, 1, &mut buf)?;

        // 创建 Cursor 用于按字节读取
        let mut cursor = VecCursor::new(buf);
        cursor.seek(SeekFrom::SeekCurrent(446))?;

        for i in 0..4 {
            table.dpte[i].flags = cursor.read_u8()?;
            table.dpte[i].starting_head = cursor.read_u8()?;
            table.dpte[i].starting_sector_cylinder = cursor.read_u16()?;
            table.dpte[i].part_type = cursor.read_u8()?;
            table.dpte[i].ending_head = cursor.read_u8()?;
            table.dpte[i].ending_sector_cylinder = cursor.read_u16()?;
            table.dpte[i].starting_lba = cursor.read_u32()?;
            table.dpte[i].total_sectors = cursor.read_u32()?;

            debug!("dpte[{i}] = {:?}", table.dpte[i]);
        }
        table.bs_trailsig = cursor.read_u16()?;
        // debug!("bs_trailsig = {}", unsafe {
        //     read_unaligned(addr_of!(table.bs_trailsig))
        // });

        if !table.is_valid() {
            return Err(SystemError::EINVAL);
        }

        return Ok(table);
    }

    /// # partitions - 获取磁盘的分区信息
    ///
    /// 该函数用于获取指定磁盘的分区信息，并将这些分区信息以分区对象的向量形式返回。分区对象包含了分区的类型、起始扇区和总扇区数等信息。
    ///
    /// ## 参数
    ///
    /// - `disk`: Weak<dyn BlockDevice>: 一个对磁盘设备的弱引用。这个磁盘设备必须实现`BlockDevice` trait。
    ///
    /// ## 返回值
    ///
    /// 返回一个包含分区信息的`Vec`。每个分区都是一个`Arc<Partition>`，它表示分区的一个强引用。
    ///
    pub fn partitions(&self, disk: Weak<dyn BlockDevice>) -> Vec<Arc<Partition>> {
        let mut partitions: Vec<Arc<Partition>> = Vec::new();
        for i in 0..4 {
            if self.dpte[i].is_valid() {
                partitions.push(Partition::new(
                    self.dpte[i].starting_sector() as u64,
                    self.dpte[i].starting_lba as u64,
                    self.dpte[i].total_sectors as u64,
                    disk.clone(),
                    i as u16,
                ));
            }
        }
        return partitions;
    }

    /// # partitions_raw - 获取磁盘的分区信息，不包含磁盘设备信息
    pub fn partitions_raw(&self) -> MbrPartitionIter {
        MbrPartitionIter::new(self)
    }

    pub fn is_valid(&self) -> bool {
        self.bs_trailsig == 0xAA55
    }
}

pub struct MbrPartitionIter<'a> {
    table: &'a MbrDiskPartionTable,
    index: usize,
}

impl<'a> MbrPartitionIter<'a> {
    fn new(table: &'a MbrDiskPartionTable) -> Self {
        MbrPartitionIter { table, index: 0 }
    }
}

impl Iterator for MbrPartitionIter<'_> {
    type Item = Partition;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < 4 {
            let entry = &self.table.dpte[self.index];
            let index = self.index;
            self.index += 1;
            if entry.is_valid() {
                let p = Partition::new_raw(
                    self.table.dpte[index].starting_sector() as u64,
                    self.table.dpte[index].starting_lba as u64,
                    self.table.dpte[index].total_sectors as u64,
                    index as u16,
                );
                return Some(p);
            }
        }
        return None;
    }
}
