use core::{ffi::c_char, mem};

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::block::{block_device::LBA_SIZE, disk_info::Partition},
    filesystem::vfs::{FileSystem, IndexNode, Metadata},
    libs::{rwlock::RwLock, spinlock::SpinLock, vec_cursor::VecCursor},
};

lazy_static! {
    pub static ref EXT2_SUPER_BLOCK: RwLock<Ex2SuperBlock> = RwLock::new(Ex2SuperBlock::default());
}

#[derive(Debug)]
pub struct Ext2FsInfo {}

impl FileSystem for Ext2FsInfo {
    fn root_inode(&self) -> alloc::sync::Arc<dyn crate::filesystem::vfs::IndexNode> {
        todo!()
    }

    fn info(&self) -> crate::filesystem::vfs::FsInfo {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }
}

// TODO 获取磁盘上的superblock并存储

// ext2超级块,大小为1024bytes
#[derive(Debug)]
#[repr(C, align(1))]
pub struct Ex2SuperBlock {
    /// 总节点数量
    pub inode_count: u32,
    /// 总数据块数量
    pub block_count: u32,
    /// 保留块数量
    pub reserved_block_count: u32,
    /// 未分配块数
    pub free_block_count: u32,
    /// 未分配结点数
    pub free_inode_count: u32,
    /// 包含超级块的数据块数
    pub first_data_block: u32,
    /// 块大小
    pub block_size: u32,
    /// 片段大小
    pub fragment_size: u32,
    /// 每组中块数量
    pub blocks_per_group: u32,
    /// 每组中片段数量
    pub fragments_per_group: u32,
    /// 每组中结点数量
    pub inodes_per_group: u32,
    /// 挂载时间
    pub mount_time: u32,
    /// 写入时间
    pub write_time: u32,
    /// 挂载次数
    pub mount_count: u16,
    /// 最大挂载次数
    pub max_mount_count: u16,
    /// ext2签名（0xef53），用于确定是否为ext2
    pub signatrue: u16,
    /// 文件系统状态
    pub state: u16,
    /// 错误操作码
    pub error_action: u16,
    /// 版本号
    pub minor_version: u16,
    /// 最后检查时间
    pub last_check_time: u32,
    /// 检查时间间隔
    pub check_interval: u32,
    /// OS
    pub os_id: u32,
    /// revision level(修订等级)
    pub major_version: u32,
    /// 保留块的默认uid
    pub def_resuid: u16,
    /// 保留块的默认gid
    pub def_resgid: u16,

    // ------extended superblock fields------
    // major version >= 1
    /// First non-reserved inode
    pub first_ino: u32,
    /* size of inode structure */
    pub inode_size: u16,
    /* block group # of this superblock */
    pub super_block_group: u16,
    /* compatible feature set */
    pub feature_compat: u32,
    /* incompatible feature set */
    pub feature_incompat: u32,
    /* readonly-compatible feature set */
    pub feature_ro_compat: u32,
    /* 128-bit uuid for volume 16*/
    pub uuid: Vec<u8>,
    /* volume name 16*/
    pub volume_name: Vec<u8>,
    /* directory where last mounted  64bytes*/
    pub last_mounted_path: Vec<u8>,
    /// algorithm for compression
    pub algorithm_usage_bitmap: u32,
    /// 为文件预分配的块数
    pub prealloc_blocks: u8,
    /// 未目录预分配的块数
    pub prealloc_dir_blocks: u8,
    padding1: u16,
    /// 日志id 16
    pub journal_uuid: Vec<u8>,
    /// 日志结点
    pub journal_inode: u32,
    /// 日志设备
    pub journal_device: u32,
    /// start of list of inodes to delete
    pub last_orphan: u32,
    /// 凑成1024B
    padding2: Vec<u32>,
}
impl Default for Ex2SuperBlock {
    fn default() -> Self {
        let uuid: Vec<u8> = vec![0; 16];
        let volume_name: Vec<u8> = vec![0; 16];
        let last_mounted_path: Vec<u8> = vec![0; 64];
        let journal_uuid: Vec<u8> = vec![0; 16];
        let padding2: Vec<u32> = vec![0; 197];
        let superblock = Self {
            uuid,
            volume_name,
            journal_uuid,
            last_mounted_path,
            padding2,
            inode_count: 0,
            block_count: 0,
            reserved_block_count: 0,
            free_block_count: 0,
            free_inode_count: 0,
            first_data_block: 0,
            block_size: 0,
            fragment_size: 0,
            blocks_per_group: 0,
            fragments_per_group: 0,
            inodes_per_group: 0,
            mount_time: 0,
            write_time: 0,
            mount_count: 0,
            max_mount_count: 0,
            signatrue: 0,
            state: 0,
            error_action: 0,
            minor_version: 0,
            last_check_time: 0,
            check_interval: 0,
            os_id: 0,
            major_version: 0,
            def_resuid: 0,
            def_resgid: 0,
            first_ino: 0,
            inode_size: 0,
            super_block_group: 0,
            feature_compat: 0,
            feature_incompat: 0,
            feature_ro_compat: 0,
            algorithm_usage_bitmap: 0,
            prealloc_blocks: 0,
            prealloc_dir_blocks: 0,
            padding1: 0,
            journal_inode: 0,
            journal_device: 0,
            last_orphan: 0,
        };
        superblock
    }
}
impl Ex2SuperBlock {
    pub fn read_superblock(partition: Arc<Partition>) -> Result<Ex2SuperBlock, SystemError> {
        let mut blc_data = Vec::with_capacity(LBA_SIZE * 2);
        blc_data.resize(LBA_SIZE * 2, 0);

        // super_block起始于volume的1024byte,并占用1024bytes
        partition.disk().read_at(
            (partition.lba_start + LBA_SIZE as u64 * 2) as usize,
            2,
            &mut blc_data,
        )?;
        let mut super_block = Ex2SuperBlock::default();

        let mut cursor = VecCursor::new(blc_data);

        // 读取super_block
        super_block.inode_count = cursor.read_u32()?;
        super_block.block_count = cursor.read_u32()?;
        super_block.reserved_block_count = cursor.read_u32()?;
        super_block.free_block_count = cursor.read_u32()?;
        super_block.free_inode_count = cursor.read_u32()?;
        super_block.first_data_block = cursor.read_u32()?;
        super_block.block_size = cursor.read_u32()?;
        super_block.fragment_size = cursor.read_u32()?;
        super_block.blocks_per_group = cursor.read_u32()?;
        super_block.fragments_per_group = cursor.read_u32()?;
        super_block.inodes_per_group = cursor.read_u32()?;
        super_block.mount_time = cursor.read_u32()?;
        super_block.write_time = cursor.read_u32()?;

        super_block.mount_count = cursor.read_u16()?;
        super_block.max_mount_count = cursor.read_u16()?;
        super_block.signatrue = cursor.read_u16()?;
        super_block.state = cursor.read_u16()?;
        super_block.error_action = cursor.read_u16()?;
        super_block.minor_version = cursor.read_u16()?;

        super_block.last_check_time = cursor.read_u32()?;
        super_block.check_interval = cursor.read_u32()?;
        super_block.os_id = cursor.read_u32()?;
        super_block.major_version = cursor.read_u32()?;

        super_block.def_resuid = cursor.read_u16()?;
        super_block.def_resgid = cursor.read_u16()?;
        // ------extended superblock fields------
        super_block.first_ino = cursor.read_u32()?;
        super_block.inode_size = cursor.read_u16()?;
        super_block.super_block_group = cursor.read_u16()?;
        super_block.feature_compat = cursor.read_u32()?;
        super_block.feature_incompat = cursor.read_u32()?;
        super_block.feature_ro_compat = cursor.read_u32()?;

        cursor.read_exact(&mut super_block.uuid)?;
        cursor.read_exact(&mut super_block.volume_name)?;
        cursor.read_exact(&mut super_block.last_mounted_path)?;

        super_block.algorithm_usage_bitmap = cursor.read_u32()?;
        super_block.prealloc_blocks = cursor.read_u8()?;
        super_block.prealloc_dir_blocks = cursor.read_u8()?;

        cursor.read_exact(&mut super_block.journal_uuid)?;
        super_block.journal_inode = cursor.read_u32()?;
        super_block.journal_device = cursor.read_u32()?;
        // FIXME 不知道会不会有问题，因为是指针，有可能需要u64
        super_block.last_orphan = cursor.read_u32()?;

        Ok(super_block)
    }
}

/// 块组描述符表(位于superblock之后)
#[derive(Debug)]
#[repr(C, align(1))]
pub struct BlockGroupDescriptor {
    block_bitmap_address: u32,
    inode_bitmap_address: u32,
    inode_table_start: u32,
    free_blocks_num: u16,
    free_inodes_num: u16,
    dir_num: u16,
    padding: Vec<u8>,
}

impl BlockGroupDescriptor {
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
}
/// 读取块组描述符表
pub fn read_block_grp_descriptor(
    partition: Arc<Partition>,
) -> Result<BlockGroupDescriptor, SystemError> {
    let mut grp_des_table = BlockGroupDescriptor::new();
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
pub struct DataBlock {
    data: [u8; 4 * 1024],
}
pub struct LockedDataBlock(RwLock<DataBlock>);
#[derive(Debug)]
pub struct LockedExt2Inode(SpinLock<Ext2Inode>);

#[derive(Debug)]
pub struct Ext2Inode {
    /// 文件类型和权限
    type_perm: u16,
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
    os_dependent_1: u32,

    /// 目录项指针
    direc_p_0: u32,
    direc_p_1: u32,
    direc_p_2: u32,
    direc_p_3: u32,
    direc_p_4: u32,
    direc_p_5: u32,
    direc_p_6: u32,
    direc_p_7: u32,
    direc_p_8: u32,
    direc_p_9: u32,
    direc_p_10: u32,
    direc_p_11: u32,

    /// 单向目录项指针
    singly_indir_p: u32,
    /// 双向目录项指针
    doubly_indir_p: u32,
    /// triply indir p
    triply_indir_p: u32,

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
}
impl Ext2Inode {}

impl LockedExt2Inode {
    pub fn get_block_group(inode: usize) -> usize {
        let inodes_per_group = EXT2_SUPER_BLOCK.read().inodes_per_group;
        return ((inode as u32 - 1) / inodes_per_group) as usize;
    }

    pub fn get_index_in_group(inode: usize) -> usize {
        let inodes_per_group = EXT2_SUPER_BLOCK.read().inodes_per_group;
        return ((inode as u32 - 1) % inodes_per_group) as usize;
    }

    pub fn get_block_addr(inode: usize) -> usize {
        let super_block = EXT2_SUPER_BLOCK.read();
        let mut inode_size = super_block.inode_size as usize;
        let mut block_size = super_block.block_size as usize;

        if super_block.major_version < 1 {
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
