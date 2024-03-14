use core::mem::{self, size_of};

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    driver::base::block::{block_device::LBA_SIZE, disk_info::Partition},
    libs::vec_cursor::VecCursor,
};

/// 块组描述符表(位于superblock之后)
#[derive(Debug)]
#[repr(C, align(1))]
pub struct Ext2BlockGroupDescriptor {
    /// 块位图的地址
    pub block_bitmap_address: u32,
    /// 节点位图的地址
    pub inode_bitmap_address: u32,
    /// 节点表的起始地址
    pub inode_table_start: u32,
    /// 空闲的块数
    pub free_blocks_num: u16,
    /// 空闲的节点数
    pub free_inodes_num: u16,
    /// 目录数
    pub dir_num: u16,
    /// 填充
    padding: Vec<u8>,
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
            padding: vec![0; 14],
        }
    }
    pub fn get_des_per_blc() -> usize {
        LBA_SIZE / mem::size_of::<Ext2BlockGroupDescriptor>()
    }
}

const EXT2_NAME_LEN: usize = 255;
pub struct Ext2DirEntry {
    /// Inode number of the file
    inode: u32,
    /// Length of the directory entry record
    record_length: u16,
    /// Length of the name
    name_length: u8,
    /// File type
    file_type: u8,
    /// Name of the file
    name: [u8; EXT2_NAME_LEN],
}

pub enum Ext2FileType {
    /// 未定义
    Unknown = 0,
    /// 普通文件
    RegularFile,
    /// 目录
    Directory,
    /// 字符设备
    CharacterDevice,
    /// 块设备
    BlockDevice,
    /// 管道
    FIFO,
    /// 套接字
    Socket,
    /// 符号链接
    Symlink,
}
