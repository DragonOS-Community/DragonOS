use alloc::sync::Arc;

use crate::{
    filesystem::vfs::{FileSystem, IndexNode},
    libs::spinlock::SpinLock,
};

use super::fs::EXT2_SB_INFO;

const EXT2_NDIR_BLOCKS: usize = 12;
const EXT2_DIND_BLOCK: usize = 13;
const EXT2_TIND_BLOCK: usize = 14;
const EXT2_BP_NUM: usize = 15;

#[derive(Debug)]
pub struct LockedExt2Inode(SpinLock<Ext2Inode>);

#[derive(Debug)]
/// 磁盘中存储的inode
pub struct Ext2Inode {
    /// 文件类型和权限
    mode: u16,
    /// 文件所有者
    uid: u16,
    /// 文件大小
    lower_size: u32,
    /// 文件访问时间
    access_time: u32,
    /// 文件创建时间
    create_time: u32,
    /// 文件修改时间
    modify_time: u32,
    /// 文件删除时间
    delete_time: u32,
    /// 文件组
    gid: u16,
    /// 文件链接数
    hard_link_num: u16,
    /// 文件在磁盘上的扇区
    disk_sector: u32,
    /// 文件属性
    flags: u32,
    // TODO 系统依赖在c++上为union
    /// 操作系统依赖
    os_dependent_1: u32,

    blocks: [u32; EXT2_BP_NUM],

    /// Generation number (Primarily used for NFS)
    generation_num: u32,

    /// In Ext2 version 0, this field is reserved.
    /// In version >= 1, Extended attribute block (File ACL).
    file_acl: u32,

    /// In Ext2 version 0, this field is reserved.
    /// In version >= 1, Upper 32 bits of file size (if feature bit set) if it's a file,
    /// Directory ACL if it's a directory
    directory_acl: u32,

    /// 片段地址
    fragment_addr: u32,
    /// 操作系统依赖
    os_dependent_2: u32,
    // TODO 系统依赖在c++上为union
}
impl Ext2Inode {}

impl LockedExt2Inode {
    pub fn get_block_group(inode: usize) -> usize {
        let sb = &EXT2_SB_INFO.read().ext2_super_block.upgrade().unwrap();
        let inodes_per_group = sb.inodes_per_group;
        return ((inode as u32 - 1) / inodes_per_group) as usize;
    }

    pub fn get_index_in_group(inode: usize) -> usize {
        let sb = &EXT2_SB_INFO.read().ext2_super_block.upgrade().unwrap();

        let inodes_per_group = sb.inodes_per_group;
        return ((inode as u32 - 1) % inodes_per_group) as usize;
    }

    pub fn get_block_addr(inode: usize) -> usize {
        let sb = &EXT2_SB_INFO.read().ext2_super_block.upgrade().unwrap();
        let mut inode_size = sb.inode_size as usize;
        let block_size = sb.block_size as usize;

        if sb.major_version < 1 {
            inode_size = 128;
        }
        return (inode * inode_size) / block_size;
    }
}

impl IndexNode for LockedExt2Inode {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn fs(&self) -> alloc::sync::Arc<dyn FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        todo!()
    }
}
