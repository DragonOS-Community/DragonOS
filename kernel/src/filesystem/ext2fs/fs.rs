use core::sync::atomic::AtomicU32;

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::block::{block_device::LBA_SIZE, disk_info::Partition},
    filesystem::vfs::{FileSystem, IndexNode},
    libs::{rwlock::RwLock, spinlock::SpinLock, vec_cursor::VecCursor},
};

use super::block_group_desc::Ext2BlockGroupDescriptor;

lazy_static! {
    pub static ref EXT2_SUPER_BLOCK: RwLock<Ex2SuperBlock> = RwLock::new(Ex2SuperBlock::default());
    pub static ref EXT2_SB_INFO: RwLock<Ext2SuperBlockInfo> =
        RwLock::new(Ext2SuperBlockInfo::default());
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
pub enum OSType {
    Linux,
    HURD,
    MASIX,
    FreeBSD,
    Lites,
}

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
    /// 包含超级块的数据块
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
    pub magic_signatrue: u16,
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
            magic_signatrue: 0,
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
z
pub struct LockSBInfo(SpinLock<Ext2SuperBlockInfo>);

#[derive(Debug, Default)]
#[repr(C, align(1))]
///second extended-fs super-block data in memory
pub struct Ext2SuperBlockInfo {
    s_frag_size: u32,                      /* Size of a fragment in bytes */
    s_frags_per_block: u32,                /* Number of fragments per block */
    s_inodes_per_block: u32,               /* Number of inodes per block */
    s_frags_per_group: u32,                /* Number of fragments in a group */
    s_blocks_per_group: u32,               /* Number of blocks in a group */
    s_inodes_per_group: u32,               /* Number of inodes in a group */
    s_itb_per_group: u32,                  /* Number of inode table blocks per group */
    s_gdb_count: u32,                      /* Number of group descriptor blocks */
    s_desc_per_block: u32,                 /* Number of group descriptors per block */
    s_groups_count: u32,                   /* Number of groups in the fs */
    s_overhead_last: u32,                  /* Last calculated overhead */
    s_blocks_last: u32,                    /* Last seen block count */
    ext2_super_block: Weak<Ex2SuperBlock>, /* Pointer to the super block in the buffer */

    group_desc_table: Weak<Vec<Ext2BlockGroupDescriptor>>,
    s_mount_opt: u32,
    s_sb_block: u32,
    s_resuid: u16,
    s_resgid: u16,
    s_mount_state: u16,
    s_pad: u16,
    /// 每个块的地址位数。
    s_addr_per_block_bits: u32,
    /// 每个块的组描述符位数。
    s_desc_per_block_bits: u32,
    s_inode_size: u32,
    /// 第一个可用的inode号
    s_first_ino: u32,
    // spinlock_t s_next_gen_lock,
    /// 下一个分配的号码
    s_next_generation: u32,
    s_dir_count: u32,
    // u8 *s_debts,
    s_freeblocks_counter: AtomicU32,
    s_freeinodes_counter: AtomicU32,
    s_dirs_counter: AtomicU32,
    /* root of the per fs reservation window tree */
    // spinlock_t s_rsv_window_lock,
    // struct rb_root s_rsv_window_root,
    // struct ext2_reserve_window_node s_rsv_window_head,
}
impl Ext2SuperBlockInfo {
    pub fn new(sb: &Ex2SuperBlock) -> Self {
        let arc_sb = Arc::new(sb.clone());
        Self {
            s_frag_size: sb.fragment_size,
            // TODO 计算
            s_frags_per_block: (),
            s_inodes_per_block: (),
            s_frags_per_group: sb.fragments_per_group,
            s_blocks_per_group: sb.blocks_per_group,
            s_inodes_per_group: sb.inodes_per_group,
            s_itb_per_group: (),
            s_gdb_count: (),
            s_desc_per_block: (),
            s_groups_count: (),
            s_overhead_last: (),
            s_blocks_last: (),
            ext2_super_block: Arc::downgrade(arc_sb),
            group_desc_table: Weak::default(),
            s_mount_opt: 0,
            s_sb_block: sb.first_data_block,
            s_resuid: sb.def_resuid,
            s_resgid: sb.def_resgid,
            s_mount_state: sb.state,
            s_pad: 0,
            s_addr_per_block_bits: (),
            s_desc_per_block_bits: (),
            s_inode_size: sb.inode_size,
            s_first_ino: sb.first_ino,
            s_next_generation: (),
            s_dir_count: (),
            s_freeblocks_counter: AtomicU32::new(sb.free_block_count),
            s_freeinodes_counter: AtomicU32::new(sb.free_inode_count),
            s_dirs_counter: AtomicU32::new(1),
        }
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
        super_block.magic_signatrue = cursor.read_u16()?;
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

    /// block group的数量
    pub fn get_group_count(&self) -> u32 {
        return (self.block_count - self.first_data_block - 1) / self.blocks_per_group + 1;
    }
    /// 块描述符所需要的块数
    pub fn get_db_count(&self) -> u32 {
        let group_count = self.get_group_count();
        let des_per_block = Ext2BlockGroupDescriptor::get_des_per_blc();
        (group_count + des_per_block - 1) / des_per_block
    }
    fn read_group_descs(
        &self,
        partition: Arc<Partition>,
    ) -> Result<Vec<Ext2BlockGroupDescriptor>, SystemError> {
        // TODO 读取块描述付
        // 需要确定读多少个
        let mut decs: Vec<Ext2BlockGroupDescriptor> = Vec::new();


    }
}

pub struct DataBlock {
    data: [u8; 4 * 1024],
}
pub struct LockedDataBlock(RwLock<DataBlock>);
