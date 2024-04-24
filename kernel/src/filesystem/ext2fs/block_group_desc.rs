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
    _padding: [u8;14],
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
