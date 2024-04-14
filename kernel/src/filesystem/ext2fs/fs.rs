use core::sync::atomic::AtomicU32;

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::block::{block_device::LBA_SIZE, disk_info::Partition},
    filesystem::{
        ext2fs::inode::Ext2Inode,
        vfs::{FileSystem, IndexNode},
    },
    libs::{rwlock::RwLock, spinlock::SpinLock, vec_cursor::VecCursor},
};

use super::{
    block_group_desc::Ext2BlockGroupDescriptor,
    inode::{Ext2InodeInfo, LockedExt2Inode, LockedExt2InodeInfo},
};

lazy_static! {
    pub static ref EXT2_SUPER_BLOCK: RwLock<Ex2SuperBlock> = RwLock::new(Ex2SuperBlock::default());
    pub static ref EXT2_SB_INFO: RwLock<Ext2SuperBlockInfo> =
        RwLock::new(Ext2SuperBlockInfo::default());
}

#[derive(Debug)]
pub struct LockedExt2SBInfo(SpinLock<Ext2SuperBlockInfo>);

#[derive(Debug)]
pub struct Ext2FileSystem {
    /// 当前文件系统所在的分区
    pub partition: Arc<Partition>,
    /// 当前文件系统的第一个数据扇区（相对分区开始位置）
    pub first_data_sector: u64,
    /// 文件系统信息结构体
    pub sb_info: Arc<LockedExt2SBInfo>,
    /// 文件系统的根inode
    root_inode: Arc<LockedExt2InodeInfo>,
}
// TODO 用于加载fs
impl FileSystem for Ext2FileSystem {
    fn root_inode(&self) -> alloc::sync::Arc<dyn crate::filesystem::vfs::IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> crate::filesystem::vfs::FsInfo {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
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

#[derive(Debug, Default)]
///second extended-fs super-block data in memory
pub struct Ext2SuperBlockInfo {
    pub s_frag_size: u32,                      /* Size of a fragment in bytes */
    pub s_frags_per_block: u32,                /* Number of fragments per block */
    pub s_inodes_per_block: u32,               /* Number of inodes per block */
    pub s_frags_per_group: u32,                /* Number of fragments in a group */
    pub s_blocks_per_group: u32,               /* Number of blocks in a group */
    pub s_inodes_per_group: u32,               /* Number of inodes in a group */
    pub s_itb_per_group: u32,                  /* Number of inode table blocks per group */
    pub s_gdb_count: u32,                      /* Number of group descriptor blocks */
    pub s_desc_per_block: u32,                 /* Number of group descriptors per block */
    pub s_groups_count: u32,                   /* Number of groups in the fs */
    pub s_overhead_last: u32,                  /* Last calculated overhead */
    pub s_blocks_last: u32,                    /* Last seen block count */
    pub ext2_super_block: Weak<Ex2SuperBlock>, /* Pointer to the super block in the buffer */

    pub group_desc_table: Weak<Vec<Ext2BlockGroupDescriptor>>,
    pub s_mount_opt: u32,
    pub s_sb_block: u32,
    pub s_resuid: u16,
    pub s_resgid: u16,
    pub s_mount_state: u16,
    pub s_pad: u16,
    /// 每个块的地址位数。
    pub s_addr_per_block_bits: u32,
    /// 每个块的组描述符位数。
    pub s_desc_per_block_bits: u32,
    /// inode大小
    pub s_inode_size: u32,
    /// 第一个可用的inode号
    pub s_first_ino: u32,
    // spinlock_t s_next_gen_lock,
    /// 下一个分配的号码
    pub s_next_generation: u32,
    pub s_dir_count: u32,
    // u8 *s_debts,
    pub s_freeblocks_counter: AtomicU32,
    pub s_freeinodes_counter: AtomicU32,
    pub s_dirs_counter: AtomicU32,

    pub partition: Weak<Partition>,
    /* root of the per fs reservation window tree */
    // spinlock_t s_rsv_window_lock,
    // struct rb_root s_rsv_window_root,
    // struct ext2_reserve_window_node s_rsv_window_head,
}

impl Ext2SuperBlockInfo {
    pub fn new(partition: Arc<Partition>) -> Self {
        let sb = Ex2SuperBlock::read_superblock(partition.clone()).unwrap();
        let global_sb = Ex2SuperBlock::read_superblock(partition.clone()).unwrap();
        let dec_table = sb.read_group_descs(partition.clone()).unwrap();
        Self {
            s_frag_size: sb.fragment_size,
            // TODO 计算
            s_frags_per_block: sb.block_size / sb.fragment_size,
            s_inodes_per_block: sb.block_size / sb.inode_size as u32,
            s_frags_per_group: sb.fragments_per_group,
            s_blocks_per_group: sb.blocks_per_group,
            s_inodes_per_group: sb.inodes_per_group,
            s_itb_per_group: 0,
            s_gdb_count: 0,
            s_desc_per_block: Ext2BlockGroupDescriptor::get_des_per_blc() as u32,
            s_groups_count: 0,
            s_overhead_last: 0,
            s_blocks_last: 0,
            ext2_super_block: Arc::downgrade(&Arc::new(global_sb)),
            group_desc_table: Arc::downgrade(&Arc::new(dec_table)),
            s_mount_opt: 0,
            s_sb_block: sb.first_data_block,
            s_resuid: sb.def_resuid,
            s_resgid: sb.def_resgid,
            s_mount_state: sb.state,
            s_pad: 0,
            s_addr_per_block_bits: 0,
            s_desc_per_block_bits: 0,
            s_inode_size: sb.inode_size as u32,
            s_first_ino: sb.first_ino,
            s_next_generation: 0,
            s_dir_count: 0,
            s_freeblocks_counter: AtomicU32::new(sb.free_block_count),
            s_freeinodes_counter: AtomicU32::new(sb.free_inode_count),
            s_dirs_counter: AtomicU32::new(1),
            partition: Arc::downgrade(&partition.clone()),
        }
    }
    /// TODO 根据索引号获取磁盘inode
    pub fn read_inode(&self, inode_index: u32) -> Result<Ext2Inode, SystemError> {
        // Get the reference to the description table
        let desc_table = self.group_desc_table.upgrade().unwrap();
        // Calculate the index of the group using the inode index
        let group_index = (inode_index - 1) / self.s_inodes_per_group;
        // 判断index是否合法
        if group_index >= desc_table.len() as u32 {
            return Err(SystemError::EINVAL);
        }
        // 获取desc_table中group_index指向的描述符
        let desc = &desc_table[group_index as usize];

        let inode_table_size = (self.s_inodes_per_group * self.s_inode_size) as usize;
        let mut inode_table_data: Vec<u8> = Vec::with_capacity(inode_table_size);
        inode_table_data.resize(inode_table_size as usize, 0);

        let idx = (inode_index - 1) % self.s_inodes_per_group;
        let pt = self.partition.upgrade().unwrap();

        // 读取inode table
        pt.disk().read_at(
            desc.inode_table_start as usize,
            inode_table_size / LBA_SIZE,
            &mut inode_table_data,
        )?;
        let mut inode_data: Vec<u8> = Vec::with_capacity(self.s_inode_size as usize);
        inode_data.resize(self.s_inode_size as usize, 0); // 读取inode table
        pt.disk().read_at(
            (desc.inode_table_start + idx * self.s_inode_size) as usize,
            1,
            &mut inode_data,
        )?;

        // // TODO 按字节获取特定inode，获取到就跳出返回
        // let mut inode_data = Vec::with_capacity(self.s_inode_size as usize);
        // inode_data.resize(self.s_inode_size as usize, 0);
        let mut cursor = VecCursor::new(inode_data);

        // let inode = Ext2Inode{
        //     mode: cursor.read_u16()?,
        //     uid: cursor.read_u16()?,
        //     lower_size: cursor.read_u32()?,
        //     access_time: cursor.read_u32()?,
        //     create_time: cursor.read_u32()?,
        //     modify_time: cursor.read_u32()?,
        //     delete_time: cursor.read_u32()?,
        //     gid: cursor.read_u16()?,
        //     hard_link_num: cursor.read_u16()?,
        //     disk_sector: cursor.read_u32()?,
        //     flags: cursor.read_u32()?,
        //     os_dependent_1: cursor.read_u32()?,
        //     blocks: cursor.re,
        //     generation_num: cursor.read_u32()?,
        //     file_acl: cursor.read_u32()?,
        //     directory_acl: cursor.read_u32()?,
        //     fragment_addr: cursor.read_u32()?,
        //     os_dependent_2: cursor.read_u16()?,
        // };
        // for _ in 0..=idx {}

        // let mut blc_data = Vec::with_capacity(LBA_SIZE);
        // blc_data.resize(LBA_SIZE, 0);
        // let mut blc_index = (inode_index - 1) * self.s_inodes_per_group + self.s_first_ino;

        // let mut blc_num = 0;
        // while blc_num < self.s_inodes_per_group {}
        todo!()
    }
    pub fn read_root_inode(&self) -> Result<Ext2Inode, SystemError> {
        let root_inode_index = 2;
        self.read_inode(root_inode_index)
    }
}

impl Ex2SuperBlock {
    // TODO 需要有个函数在加载的时候read superblock and read des table ，并且读root inode
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
    pub fn get_group_count(&self) -> usize {
        return ((self.block_count - self.first_data_block - 1) / self.blocks_per_group + 1)
            as usize;
    }
    /// 块描述符所需要的块数
    pub fn get_db_count(&self) -> usize {
        let group_count = self.get_group_count();
        let des_per_block = Ext2BlockGroupDescriptor::get_des_per_blc();
        (group_count + des_per_block - 1) / des_per_block
    }
    fn read_group_descs(
        &self,
        partition: Arc<Partition>,
    ) -> Result<Vec<Ext2BlockGroupDescriptor>, SystemError> {
        // 先确定块数，再遍历块，再n个字节n个字节读
        let db_count = self.get_db_count();
        let des_per_block = Ext2BlockGroupDescriptor::get_des_per_blc();
        // 需要确定读多少个
        let mut decs: Vec<Ext2BlockGroupDescriptor> = Vec::with_capacity(db_count * des_per_block);

        let mut blc_data = Vec::with_capacity(LBA_SIZE * db_count);
        blc_data.resize(LBA_SIZE * db_count, 0);

        partition.disk().read_at(
            (partition.lba_start + (LBA_SIZE * db_count) as u64) as usize,
            db_count,
            &mut blc_data,
        )?;
        let mut cursor = VecCursor::new(blc_data);
        for _ in 0..db_count {
            let mut d = Ext2BlockGroupDescriptor::new();
            d.block_bitmap_address = cursor.read_u32()?;
            d.inode_bitmap_address = cursor.read_u32()?;
            d.inode_table_start = cursor.read_u32()?;
            d.free_blocks_num = cursor.read_u16()?;
            d.free_inodes_num = cursor.read_u16()?;
            d.dir_num = cursor.read_u16()?;

            decs.push(d);
        }
        Ok(decs)
    }
}
