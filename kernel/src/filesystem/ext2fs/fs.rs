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

lazy_static! {}

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
    pub minor_revision_level: u16,
    /// 最后检查时间
    pub last_check_time: u32,
    /// 检查时间间隔
    pub check_interval: u32,
    /// OS
    pub os_id: u32,
    /// revision level(修订等级)
    pub revision_level: u32,
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
    pub volume_name: Vec<c_char>,
    /* directory where last mounted  64bytes*/
    pub last_mounted_path: Vec<c_char>,
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

    padding2: Vec<u32>,
}
impl Default for Ex2SuperBlock {
    fn default() -> Self {
        Self {
            // TODO 分配指定内存
            ..Default::default()
        }
    }
}
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
    // TODO 读取数据填充
    Ok(super_block)
}
pub struct DataBlock {
    daat: [u8; 4 * 1024],
}
pub struct LockedDataBlock(RwLock<DataBlock>);
#[derive(Debug)]
pub struct LockedExt2Inode(SpinLock<Ext2Inode>);

#[derive(Debug)]
pub struct Ext2Inode {
    /// 指向自身的弱引用
    self_ref: Weak<LockedExt2Inode>,

    /// 当前inode的元数据
    metadata: Metadata,

    /// TODO 一级指针 12个
    direct_block0: Weak<LockedDataBlock>,

    /// TODO 二级指针 1
    indirect_block: Weak<LockedExt2Inode>,
    /// TODO 三级指针 1
    three_level_block: Weak<LockedExt2Inode>,
}

impl Ext2Inode {}

impl LockedExt2Inode {}

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
