use core::{fmt::Debug, mem::ManuallyDrop};

use alloc::{rc::Weak, sync::Arc, vec::Vec};

use crate::{
    filesystem::vfs::{FileSystem, IndexNode, Metadata},
    libs::{rwlock::RwLock, spinlock::SpinLock},
};

use super::fs::EXT2_SB_INFO;

const EXT2_NDIR_BLOCKS: usize = 12;
const EXT2_DIND_BLOCK: usize = 13;
const EXT2_TIND_BLOCK: usize = 14;
const EXT2_BP_NUM: usize = 15;

#[derive(Debug)]
pub struct LockedExt2Inode(SpinLock<Ext2Inode>);

/// inode中根据不同系统的保留值
#[repr(C, align(1))]
pub union OSD1 {
    linux_reserved: u32,
    hurd_tanslator: u32,
    masix_reserved: u32,
}
impl Debug for OSD1 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "OSD1:{}", unsafe { self.linux_reserved })
    }
}
#[derive(Debug)]
#[repr(C, align(1))]
struct MasixOsd2 {
    frag_num: u8,
    frag_size: u8,
    pad: u16,
    reserved: [u32; 2],
}
#[derive(Debug)]
#[repr(C, align(1))]
struct LinuxOsd2 {
    frag_num: u8,
    frag_size: u8,
    pad: u16,
    uid_high: u16,
    gid_high: u16,
    reserved: u32,
}

#[repr(C, align(1))]
#[derive(Debug)]
struct HurdOsd2 {
    frag_num: u8,
    frag_size: u8,
    mode_high: u16,
    uid_high: u16,
    gid_high: u16,
    author: u32,
}

/// inode中根据不同系统的保留值
#[repr(C, align(1))]
pub union OSD2 {
    linux: ManuallyDrop<LinuxOsd2>,
    hurd: ManuallyDrop<HurdOsd2>,
    masix: ManuallyDrop<MasixOsd2>,
}
impl Debug for OSD2 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "OSD2:{:?}", unsafe { &self.linux })
    }
}
#[derive(Debug)]
#[repr(C, align(1))]
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
    /// 操作系统依赖
    os_dependent_1: OSD1,

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
    os_dependent_2: OSD2,
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
pub struct DataBlock {
    data: [u8; 4 * 1024],
}
pub struct LockedDataBlock(RwLock<DataBlock>);

pub struct Indirect {
    pub self_ref: Weak<Indirect>,
    pub next_point: Vec<Option<Arc<Indirect>>>,
    pub data_block: Option<Arc<DataBlock>>,
}
#[derive(Debug)]
pub struct LockedExt2InodeInfo(SpinLock<Ext2InodeInfo>);

#[derive(Debug)]
/// 存储在内存中的inode
pub struct Ext2InodeInfo {
    // TODO 将ext2iode内容和meta联系在一起，可自行设计
    meta: Metadata,
}

impl Ext2InodeInfo {
    pub fn new(inode: Ext2Inode) -> Self {
        // TODO 初始化inode info
        todo!()
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
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        todo!()
    }
}
