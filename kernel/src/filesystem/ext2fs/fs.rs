use core::{mem, sync::atomic::AtomicU32};

use alloc::{
    fmt,
    sync::{Arc, Weak},
    vec::Vec,
};
use log::{debug, error};
use system_error::SystemError;

use crate::{
    driver::base::block::{
        block_device::{__bytes_to_lba, LBA_SIZE},
        disk_info::Partition,
    },
    filesystem::{
        ext2fs::inode::Ext2Inode,
        vfs::{FileSystem, IndexNode, ROOT_INODE},
    },
    libs::{rwlock::RwLock, spinlock::SpinLock, vec_cursor::VecCursor},
};

use super::{
    block_group_desc::Ext2BlockGroupDescriptor,
    inode::{Ext2InodeInfo, LockedExt2Inode, LockedExt2InodeInfo},
};
use core::fmt::Debug;
lazy_static! {
    // pub static ref EXT2_SUPER_BLOCK: RwLock<Ext2SuperBlock> =
    //     RwLock::new(Ext2SuperBlock::default());
    pub static ref EXT2_SB_INFO: RwLock<Ext2SuperBlockInfo> =
        RwLock::new(Ext2SuperBlockInfo::default());
}
// pub static ref EXT2_FS: RwLock<Ext2FileSystem> = unsafe{U};

#[derive(Debug)]
pub struct LockedExt2SBInfo(pub SpinLock<Ext2SuperBlockInfo>);

#[derive(Debug)]
pub struct Ext2FileSystem {
    // TODO 考虑将group descriptor table也放在里面
    /// 当前文件系统所在的分区
    pub partition: Arc<Partition>,
    /// 当前文件系统的第一个数据扇区（相对分区开始位置）
    pub first_data_sector: u64,
    /// 文件系统信息结构体
    pub sb_info: Arc<LockedExt2SBInfo>,
    /// 文件系统的根inode
    root_inode: Arc<LockedExt2InodeInfo>,
    // TODO 做一个缓存机制 记录inode
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

    fn name(&self) -> &str {
        todo!()
    }

    fn super_block(&self) -> crate::filesystem::vfs::SuperBlock {
        todo!()
    }
}

impl Ext2FileSystem {
    pub fn new(partition: Arc<Partition>) -> Result<Arc<Ext2FileSystem>, SystemError> {
        // TODO 冗余
        // let sb = Ext2SuperBlock::read_superblock(partition.clone())
        //     .map_err(|err| {
        //         error!("ext2 mount failed, because read superblock failed");
        //         return Err(err);
        //     })
        //     .unwrap();
        // let gpd_table = sb.read_group_descs(partition.clone()).map_err(|err| {
        //     error!("ext2 mount failed, because read group descs failed");
        //     return Err(err);
        // });

        // TODO 读取superblock，实现挂载
        // debug!("begin mount Ext2FS");
        let sb_info = Ext2SuperBlockInfo::new(partition.clone());
        let root_inode = sb_info.read_root_inode();
        if root_inode.is_err() {
            error!("ext2 mount failed, because read root inode failed");
            return Err(root_inode.err().unwrap());
        }

        let root_inode = root_inode.unwrap();
        // debug!("new the Ext2InodeInfo");
        let r_info = Ext2InodeInfo::new(&root_inode,2);
        // debug!("end mount Ext2FS");
        return Ok(Arc::new(Self {
            partition,
            first_data_sector: 0,
            sb_info: Arc::new(LockedExt2SBInfo(SpinLock::new(sb_info))),
            root_inode: Arc::new(LockedExt2InodeInfo(SpinLock::new(r_info))),
        }));
    }
    pub fn super_block(&self) -> Arc<LockedExt2SBInfo> {
        self.sb_info.clone()
    }
    pub fn get_block_size(&self) -> usize {
        self.sb_info.0.lock().s_block_size as usize
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
#[derive(Clone)]
#[repr(C, align(1))]
pub struct Ext2SuperBlock {
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
    /// 块大小偏移量 1024 << block_size
    pub block_size: u32,
    /// 片段大小偏移量 1024 << fragment_size
    pub fragment_size: i32,
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
    /// inode大小
    pub inode_size: u16,
    /// block group # of this superblock
    pub super_block_group: u16,
    /// 兼容功能集
    pub feature_compat: u32,
    /// 不兼容的功能集
    pub feature_incompat: u32,
    /// 只读兼容功能集
    pub feature_ro_compat: u32,
    /// 卷的uuid
    pub uuid: Vec<u8>,
    /// 卷名
    pub volume_name: Vec<u8>,
    /// 上一次挂载的路径
    pub last_mounted_path: Vec<u8>,
    /// 压缩算法位图
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
    /// 要删除的inode链表头
    pub last_orphan: u32,
    /// 凑成1024B
    padding2: Vec<u32>,
}

impl Default for Ext2SuperBlock {
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
impl Ext2SuperBlock {
    pub fn get_block_size(&self) -> usize {
        1024usize << self.block_size
    }
}
#[derive(Debug, Default)]
///second extended-fs super-block data in memory
pub struct Ext2SuperBlockInfo {
    pub s_frag_size: u32, /* Size of a fragment in bytes */
    pub s_block_size: u32,
    pub s_frags_per_block: u32,  /* Number of fragments per block */
    pub s_inodes_per_block: u32, /* Number of inodes per block */
    pub s_frags_per_group: u32,  /* Number of fragments in a group */
    pub s_blocks_per_group: u32, /* Number of blocks in a group */
    pub s_inodes_per_group: u32, /* Number of inodes in a group */
    pub s_itb_per_group: u32,    /* Number of inode table blocks per group */
    pub s_gdb_count: u32,        /* Number of group descriptor blocks */
    pub s_desc_per_block: u32,   /* Number of group descriptors per block */
    pub s_groups_count: u32,     /* Number of groups in the fs */
    pub s_overhead_last: u32,    /* Last calculated overhead */
    pub s_blocks_last: u32,      /* Last seen block count */
    pub ext2_super_block: Option<Arc<Ext2SuperBlock>>, /* Pointer to the super block in the buffer */

    pub group_desc_table: Option<Arc<Vec<Ext2BlockGroupDescriptor>>>,
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

    pub partition: Option<Arc<Partition>>, /* root of the per fs reservation window tree */
                                           // spinlock_t s_rsv_window_lock,
                                           // struct rb_root s_rsv_window_root,
                                           // struct ext2_reserve_window_node s_rsv_window_head,
    pub major_version:u32,
    pub file_pre_alloc:u8,
}

impl Ext2SuperBlockInfo {
    pub fn new(partition: Arc<Partition>) -> Self {
        let sb = Ext2SuperBlock::read_superblock(partition.clone()).unwrap();
        // let global_sb = Ext2SuperBlock::read_superblock(partition.clone()).unwrap();
        let global_sb = sb.clone();
        let dec_table = sb.read_group_descs(partition.clone()).unwrap();
        let block_size: u32 = 1024 << sb.block_size;
        let fragment_size: u32 = if sb.fragment_size >= 0 {
            1024 << sb.fragment_size
        } else {
            1024 >> -sb.fragment_size
        };
        let inode_size = if sb.inode_size > 0 {
            sb.inode_size
        } else {
            128
        };
        
        debug!(
            "fragment_size = {},block_size = {}",
            fragment_size,
            block_size
        );
        debug!(
            "s_inodes_per_block = {}.s_frags_per_block = {}",
            block_size / inode_size as u32,
            block_size / fragment_size
        );
        let ret = Self {
            s_frag_size: fragment_size as u32,
            // TODO 计算
            s_block_size: block_size as u32,
            s_frags_per_block: block_size / fragment_size,
            s_inodes_per_block: block_size / inode_size as u32,
            s_frags_per_group: sb.fragments_per_group,
            s_blocks_per_group: sb.blocks_per_group,
            s_inodes_per_group: sb.inodes_per_group,
            s_itb_per_group: 0,
            s_gdb_count: 0,
            s_desc_per_block: Ext2BlockGroupDescriptor::get_des_per_blc() as u32,
            s_groups_count: 0,
            s_overhead_last: 0,
            s_blocks_last: 0,
            ext2_super_block: Some(Arc::new(global_sb)),
            group_desc_table: Some(Arc::new(dec_table)),
            s_mount_opt: 0,
            s_sb_block: sb.first_data_block,
            s_resuid: sb.def_resuid,
            s_resgid: sb.def_resgid,
            s_mount_state: sb.state,
            s_pad: 0,
            s_addr_per_block_bits: 0,
            s_desc_per_block_bits: 0,
            s_inode_size: inode_size as u32,
            s_first_ino: sb.first_ino,
            s_next_generation: 0,
            s_dir_count: 0,
            s_freeblocks_counter: AtomicU32::new(sb.free_block_count),
            s_freeinodes_counter: AtomicU32::new(sb.free_inode_count),
            s_dirs_counter: AtomicU32::new(1),
            partition: Some(partition.clone()),
            major_version: sb.major_version,
            file_pre_alloc:sb.prealloc_blocks,
        };
        // debug!("end build super block info");
        ret
    }
    ///  根据索引号获取磁盘inode
    ///
    pub fn read_inode(&self, inode_index: u32) -> Result<Ext2Inode, SystemError> {
        // kinfo!("begin read inode");
        // Get the reference to the description table
        let desc_table = self.group_desc_table.clone();
        if desc_table.is_none() {
            // debug!("descriptor table is empty");
            return Err(SystemError::EINVAL);
        }
        let desc_table = desc_table.as_ref().unwrap();
        // Calculate the index of the group using the inode index
        let group_index = (inode_index - 1) / self.s_inodes_per_group;
        // 判断index是否合法
        if group_index >= desc_table.len() as u32 {
            return Err(SystemError::EINVAL);
        }
        // 获取desc_table中group_index指向的描述符
        let desc = &desc_table[group_index as usize];

        // inode table 起始块号
        let mut inode_table_lba_id = desc.inode_table_start as usize * 2;
        let idx = ((inode_index - 1) % self.s_inodes_per_group) as usize;
        inode_table_lba_id += idx * 2 / (self.s_inodes_per_block as usize);
        // inode table 块数量
        let mut read_lba_num = self.s_inodes_per_group as usize / self.s_inodes_per_block as usize;

        if self.s_inodes_per_group as usize % self.s_inodes_per_block as usize != 0 {
            read_lba_num += 1;
        }
        read_lba_num *= 2;

        // // debug!("read_lba_num = {read_lba_num}");
        // inode table 数据，存储的是inode数组
        let mut inode_table_data: Vec<u8> = Vec::with_capacity(LBA_SIZE);
        inode_table_data.resize(LBA_SIZE, 0);

        let pt = self.partition.as_ref().unwrap();
        let ret = pt.disk().read_at(
            inode_table_lba_id,
            // read_lba_num,
            1,
            inode_table_data.as_mut_slice(),
        );
        if ret.is_err() {
            error!("read ext2 {inode_index} inode failed");
            return Err(ret.err().unwrap());
        }
        let new_pos = (idx % 4) * self.s_inode_size as usize;
        let inode = Ext2Inode::new_from_bytes(
            &inode_table_data[new_pos..new_pos + self.s_inode_size as usize].to_vec(),
        );
        if inode.is_err() {
            error!("ext2 {inode_index} inode Ext2Inode::new_from_bytes failed");
            return Err(inode.err().unwrap());
        }
        let inode = inode.unwrap();
        // let inode_data: Vec<Ext2Inode> =
        //     unsafe { core::mem::transmute::<Vec<u8>, Vec<Ext2Inode>>(inode_table_data) };
        // let inode = inode_data[idx % 4 as usize].clone();
        // drop(inode_data);
        // kinfo!("end read inode");

        // // debug!("{:?}", inode);

        return Ok(inode);
    }

    pub fn read_root_inode(&self) -> Result<Ext2Inode, SystemError> {
        // kinfo!("begin read root inode");
        let root_inode_index = 2;
        // let root_inode = self.read_inode(root_inode_index);
        match self.read_inode(root_inode_index) {
            Ok(root_inode) => {
                // debug!("root inode = {:?}", root_inode);
                // kinfo!("end read root inode");
                return Ok(root_inode);
            }
            Err(err) => {
                error!("failed to read root index,{:?}", err);
                return Err(err);
            }
        }
    }
}

impl Ext2SuperBlock {
    // TODO 需要有个函数在加载的时候read superblock and read des table ，并且读root inode
    pub fn read_superblock(partition: Arc<Partition>) -> Result<Ext2SuperBlock, SystemError> {
        // debug!("begin read superblock");
        let mut blc_data = Vec::with_capacity(LBA_SIZE * 2);
        blc_data.resize(LBA_SIZE * 2, 0);

        // super_block起始于volume的1024byte,并占用1024bytes
        partition.disk().read_at(2usize, 2, &mut blc_data)?;
        let mut super_block = Ext2SuperBlock::default();

        let mut cursor = VecCursor::new(blc_data);

        // 读取super_block
        super_block.inode_count = cursor.read_u32()?;
        super_block.block_count = cursor.read_u32()?;
        super_block.reserved_block_count = cursor.read_u32()?;
        super_block.free_block_count = cursor.read_u32()?;
        super_block.free_inode_count = cursor.read_u32()?;
        super_block.first_data_block = cursor.read_u32()?;
        super_block.block_size = cursor.read_u32()?;
        let mut log_f_size = [0u8; 4];
        cursor.read_exact(&mut log_f_size)?;
        super_block.fragment_size = i32::from_le_bytes(log_f_size);
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

        super_block.last_orphan = cursor.read_u32()?;
        // debug!("{:?}", super_block);
        // debug!("end read superblock");

        Ok(super_block)
    }

    /// block group的数量
    pub fn get_group_count(&self) -> usize {
        debug!(
            "block_count = {},blocks_per_group = {}",
            self.block_count,
            self.blocks_per_group
        );
        return (self.block_count / self.blocks_per_group) as usize;
    }
    /// 块描述符所需要的块数
    pub fn get_db_count(&self) -> usize {
        let group_count = self.get_group_count();
        // let des_per_block = Ext2BlockGroupDescriptor::get_des_per_blc();
        let block_size: usize = 1024 << self.block_size;
        let des_per_block = block_size / mem::size_of::<Ext2BlockGroupDescriptor>() as usize;
        let size = mem::size_of::<Ext2BlockGroupDescriptor>() as usize;
        // debug!("group_count = {group_count},des_per_block = {des_per_block},size = {size}");
        // (group_count + des_per_block - 1) / des_per_block
        (group_count * size) / block_size
    }
    pub fn read_group_descs(
        &self,
        partition: Arc<Partition>,
    ) -> Result<Vec<Ext2BlockGroupDescriptor>, SystemError> {
        // debug!("begin read group descriptors");

        // 先确定块数，再遍历块，再n个字节n个字节读
        let group_count = (self.block_count / self.blocks_per_group) as usize;
        let block_size: usize = 1024 << self.block_size;
        let size = mem::size_of::<Ext2BlockGroupDescriptor>() as usize;
        let des_per_block = block_size / size;
        debug!(
            "group_count = {},des_per_block = {des_per_block},size = {size}",
            group_count + 1
        );

        let mut db_count = (group_count * size) / block_size;
        if (group_count * size) % block_size != 0 {
            db_count += 1;
        }
        // let des_per_block = Ext2BlockGroupDescriptor::get_des_per_blc();

        let total_des = des_per_block * db_count;
        // 需要确定读多少个
        let mut decs: Vec<Ext2BlockGroupDescriptor> = Vec::with_capacity(db_count * des_per_block);

        let mut blc_data = Vec::with_capacity(block_size * db_count);
        blc_data.resize(block_size * db_count, 0);
        // debug!("dbcount = {db_count},block_size = {block_size},des_per_block = {des_per_block},total_des = {total_des},blc_data.len={:?}",blc_data.len());
        partition
            .disk()
            .read_at(4usize, db_count * 2, &mut blc_data)?;
        let mut cursor = VecCursor::new(blc_data);
        for _ in 0..total_des {
            let mut d = Ext2BlockGroupDescriptor::new();
            d.block_bitmap_address = cursor.read_u32()?;
            d.inode_bitmap_address = cursor.read_u32()?;
            d.inode_table_start = cursor.read_u32()?;
            if d.inode_table_start == 0 {
                break;
            }
            d.free_blocks_num = cursor.read_u16()?;
            d.free_inodes_num = cursor.read_u16()?;
            d.dir_num = cursor.read_u16()?;
            let mut bg_flags = [0u8; 14];
            cursor.read_exact(&mut bg_flags)?;

            // debug!("{:?}", d);
            decs.push(d);
        }
        // debug!("end read group descriptors");

        Ok(decs)
    }
}

impl Debug for Ext2SuperBlock {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ext2SuperBlock")
            .field("inode_count", &format_args!("{}\n", &self.inode_count))
            .field("block_count", &format_args!("{}\n", &self.block_count))
            .field(
                "reserved_block_count",
                &format_args!("{}\n", &self.reserved_block_count),
            )
            .field(
                "free_block_count",
                &format_args!("{}\n", &self.free_block_count),
            )
            .field(
                "free_inode_count",
                &format_args!("{}\n", &self.free_inode_count),
            )
            .field(
                "first_data_block",
                &format_args!("{}\n", &self.first_data_block),
            )
            .field("block_size", &format_args!("{}\n", &self.block_size))
            .field("fragment_size", &format_args!("{}\n", &self.fragment_size))
            .field(
                "blocks_per_group",
                &format_args!("{}\n", &self.blocks_per_group),
            )
            .field(
                "fragments_per_group",
                &format_args!("{}\n", &self.fragments_per_group),
            )
            .field(
                "inodes_per_group",
                &format_args!("{}\n", &self.inodes_per_group),
            )
            .field("mount_time", &format_args!("{}\n", &self.mount_time))
            .field("write_time", &format_args!("{}\n", &self.write_time))
            .field("mount_count", &format_args!("{}\n", &self.mount_count))
            .field(
                "max_mount_count",
                &format_args!("{}\n", &self.max_mount_count),
            )
            .field(
                "magic_signatrue",
                &format_args!("{}\n", &self.magic_signatrue),
            )
            .field("state", &format_args!("{}\n", &self.state))
            .field("error_action", &format_args!("{}\n", &self.error_action))
            .field("minor_version", &format_args!("{}\n", &self.minor_version))
            .field(
                "last_check_time",
                &format_args!("{}\n", &self.last_check_time),
            )
            .field(
                "check_interval",
                &format_args!("{}\n", &self.check_interval),
            )
            .field("os_id", &format_args!("{}\n", &self.os_id))
            .field("major_version", &format_args!("{}\n", &self.major_version))
            .field("def_resuid", &format_args!("{}\n", &self.def_resuid))
            .field("def_resgid", &format_args!("{}\n", &self.def_resgid))
            .field("first_ino", &format_args!("{}\n", &self.first_ino))
            .field("inode_size", &format_args!("{}\n", &self.inode_size))
            .field(
                "super_block_group",
                &format_args!("{}\n", &self.super_block_group),
            )
            .field(
                "feature_compat",
                &format_args!("{}\n", &self.feature_compat),
            )
            .field(
                "feature_incompat",
                &format_args!("{}\n", &self.feature_incompat),
            )
            .field(
                "feature_ro_compat",
                &format_args!("{}\n", &self.feature_ro_compat),
            )
            .field("uuid", &format_args!("\n{:?}", &self.uuid))
            .field("volume_name", &format_args!("\n{:?}", &self.volume_name))
            .field(
                "last_mounted_path",
                &format_args!("\n{:?}", &self.last_mounted_path),
            )
            .field(
                "algorithm_usage_bitmap",
                &format_args!("{}\n", &self.algorithm_usage_bitmap),
            )
            .field(
                "prealloc_blocks",
                &format_args!("{}\n", &self.prealloc_blocks),
            )
            .field(
                "prealloc_dir_blocks",
                &format_args!("{}\n", &self.prealloc_dir_blocks),
            )
            .field("journal_uuid", &format_args!("\n{:?}", &self.journal_uuid))
            .field("journal_inode", &format_args!("{}\n", &self.journal_inode))
            .field(
                "journal_device",
                &format_args!("{}\n", &self.journal_device),
            )
            .field("last_orphan", &format_args!("{}\n", &self.last_orphan))
            .finish()
    }
}
