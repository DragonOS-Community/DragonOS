use core::mem::{self, size_of};

use alloc::{fmt, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::driver::base::block::{block_device::LBA_SIZE, disk_info::Partition};
use core::fmt::Debug;
/// 块组描述符表(位于superblock之后)
#[repr(C, align(1))]
pub struct Ext2BlockGroupDescriptor {
    /// 块位图的地址
    pub block_bitmap_address: u32,
    /// 节点位图的地址
    pub inode_bitmap_address: u32,
    /// 指向inode table的指针
    pub inode_table_start: u32,
    /// 空闲的块数
    pub free_blocks_num: u16,
    /// 空闲的节点数
    pub free_inodes_num: u16,
    /// 目录数
    pub dir_num: u16,
    /// 填充
    _padding: [u8; 14],
}

impl Ext2BlockGroupDescriptor {
    pub fn new() -> Self {
        Self {
            block_bitmap_address: 0,
            inode_bitmap_address: 0,
            inode_table_start: 0,
            free_blocks_num: 0,
            free_inodes_num: 0,
            dir_num: 0,
            _padding: [0; 14],
        }
    }
    pub fn get_des_per_blc() -> usize {
        LBA_SIZE / mem::size_of::<Ext2BlockGroupDescriptor>()
    }
    // TODO 读取inode
    pub fn flush(
        &self,
        partition: &Arc<Partition>,
        group_id: usize,
        block_size: usize,
    ) -> Result<(), SystemError> {
        let offset = group_id * mem::size_of::<Ext2BlockGroupDescriptor>();
        let count = block_size / LBA_SIZE;
        let start_block = ((2048 + offset) / block_size) * count;
        let mut des_data: Vec<u8> = Vec::with_capacity(LBA_SIZE * count);
        des_data.resize(LBA_SIZE * count, 0);
        let _ = partition.disk().read_at(start_block, count, &mut des_data);
        let offset_in_block = offset % block_size;
        des_data[offset_in_block..offset_in_block + mem::size_of::<Ext2BlockGroupDescriptor>()]
            .copy_from_slice(self.to_bytes().as_slice());
        let _ = partition.disk().write_at(start_block, count, &mut des_data);

        Ok(())
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(mem::size_of::<Ext2BlockGroupDescriptor>());
        bytes.extend_from_slice(&self.block_bitmap_address.to_le_bytes());
        bytes.extend_from_slice(&self.inode_bitmap_address.to_le_bytes());
        bytes.extend_from_slice(&self.inode_table_start.to_le_bytes());
        bytes.extend_from_slice(&self.free_blocks_num.to_le_bytes());
        bytes.extend_from_slice(&self.free_inodes_num.to_le_bytes());
        bytes.extend_from_slice(&self.dir_num.to_le_bytes());
        bytes
    }
}

impl Debug for Ext2BlockGroupDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ext2BlockGroupDescriptor")
            .field(
                "block_bitmap_address",
                &format_args!("{:?}\n", &self.block_bitmap_address),
            )
            .field(
                "inode_bitmap_address",
                &format_args!("{:?}\n", &self.inode_bitmap_address),
            )
            .field(
                "inode_table_start",
                &format_args!("{:?}\n", &self.inode_table_start),
            )
            .field(
                "free_blocks_num",
                &format_args!("{:?}\n", &self.free_blocks_num),
            )
            .field(
                "free_inodes_num",
                &format_args!("{:?}\n", &self.free_inodes_num),
            )
            .field("dir_num", &format_args!("{:?}\n", &self.dir_num))
            .finish()
    }
}

pub fn get_group_id(inode_num: usize, i_per_group: usize) -> usize {
    inode_num / i_per_group
}
pub fn get_index_in_group(inode_num: usize, i_per_group: usize) -> usize {
    inode_num % i_per_group
}
