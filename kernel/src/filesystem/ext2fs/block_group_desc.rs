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
    block_bitmap_address: u32,
    /// 节点位图的地址
    inode_bitmap_address: u32,
    /// 节点表的起始地址
    inode_table_start: u32,
    /// 空闲的块数
    free_blocks_num: u16,
    /// 空闲的节点数
    free_inodes_num: u16,
    /// 目录数
    dir_num: u16,
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
    pub fn get_des_per_blc() -> u32 {
        LBA_SIZE / mem::size_of::<Ext2BlockGroupDescriptor>()
    }
}
/// 读取块组描述符表
pub fn read_block_grp_descriptor(
    partition: Arc<Partition>,
) -> Result<Ext2BlockGroupDescriptor, SystemError> {
    // TODO 要计算读多少个块，读到一个数组里面

    let mut grp_des_table = Ext2BlockGroupDescriptor::new();
    let mut data: Vec<u8> = Vec::with_capacity(LBA_SIZE);
    data.resize(LBA_SIZE, 0);
    partition.disk().read_at(
        (partition.lba_start + LBA_SIZE as u64 * 2) as usize,
        1,
        &mut data,
    )?;
    let mut cursor = VecCursor::new(data);
    grp_des_table.block_bitmap_address = cursor.read_u32()?;
    grp_des_table.inode_bitmap_address = cursor.read_u32()?;
    grp_des_table.inode_table_start = cursor.read_u32()?;
    grp_des_table.free_blocks_num = cursor.read_u16()?;
    grp_des_table.free_inodes_num = cursor.read_u16()?;
    grp_des_table.dir_num = cursor.read_u16()?;

    Ok(grp_des_table)
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
