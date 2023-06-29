#![allow(dead_code)]
use core::{any::Any, fmt::Debug};

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    filesystem::vfs::{
        core::generate_inode_id,
        file::{FileMode, FilePrivateData},
        FileSystem, FileType, IndexNode, InodeId, Metadata, PollStatus,
    },
    io::{device::LBA_SIZE, disk_info::Partition, SeekFrom},
    kerror,
    libs::{
        spinlock::{SpinLock, SpinLockGuard},
        vec_cursor::VecCursor,
    },
    syscall::SystemError,
    time::TimeSpec,
};

use super::{
    bpb::{BiosParameterBlock, FATType},
    entry::{FATDir, FATDirEntry, FATDirIter, FATEntry},
    utils::RESERVED_CLUSTERS,
};

/// FAT32文件系统的最大的文件大小
pub const MAX_FILE_SIZE: u64 = 0xffff_ffff;

/// @brief 表示当前簇和上一个簇的关系的结构体
/// 定义这样一个结构体的原因是，FAT文件系统的文件中，前后两个簇具有关联关系。
#[derive(Debug, Clone, Copy, Default)]
pub struct Cluster {
    pub cluster_num: u64,
    pub parent_cluster: u64,
}

impl PartialOrd for Cluster {
    /// @brief 根据当前簇号比较大小
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        return self.cluster_num.partial_cmp(&other.cluster_num);
    }
}

impl PartialEq for Cluster {
    /// @brief 根据当前簇号比较是否相等
    fn eq(&self, other: &Self) -> bool {
        self.cluster_num == other.cluster_num
    }
}

impl Eq for Cluster {}

#[derive(Debug)]
pub struct FATFileSystem {
    /// 当前文件系统所在的分区
    pub partition: Arc<Partition>,
    /// 当前文件系统的BOPB
    pub bpb: BiosParameterBlock,
    /// 当前文件系统的第一个数据扇区（相对分区开始位置）
    pub first_data_sector: u64,
    /// 文件系统信息结构体
    pub fs_info: Arc<LockedFATFsInfo>,
    /// 文件系统的根inode
    root_inode: Arc<LockedFATInode>,
}

/// FAT文件系统的Inode
#[derive(Debug)]
pub struct LockedFATInode(SpinLock<FATInode>);

#[derive(Debug)]
pub struct LockedFATFsInfo(SpinLock<FATFsInfo>);

impl LockedFATFsInfo {
    #[inline]
    pub fn new(fs_info: FATFsInfo) -> Self {
        return Self(SpinLock::new(fs_info));
    }
}

#[derive(Debug)]
pub struct FATInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedFATInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedFATInode>,
    /// 子Inode的B树. 该数据结构用作缓存区。其中，它的key表示inode的名称。
    /// 请注意，由于FAT的查询过程对大小写不敏感，因此我们选择让key全部是大写的，方便统一操作。
    children: BTreeMap<String, Arc<LockedFATInode>>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<FATFileSystem>,

    /// 根据不同的Inode类型，创建不同的私有字段
    inode_type: FATDirEntry,
}

impl FATInode {
    /// @brief 更新当前inode的元数据
    pub fn update_metadata(&mut self) {
        // todo: 更新文件的访问时间等信息
        match &self.inode_type {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                self.metadata.size = f.size() as i64;
            }
            FATDirEntry::Dir(d) => {
                self.metadata.size = d.size(&self.fs.upgrade().unwrap().clone()) as i64;
            }
            FATDirEntry::UnInit => {
                kerror!("update_metadata: Uninitialized FATDirEntry: {:?}", self);
                return;
            }
        };
    }

    fn find(&mut self, name: &str) -> Result<Arc<LockedFATInode>, SystemError> {
        match &self.inode_type {
            FATDirEntry::Dir(d) => {
                // 尝试在缓存区查找
                if let Some(entry) = self.children.get(&name.to_uppercase()) {
                    return Ok(entry.clone());
                }
                // 在缓存区找不到
                // 在磁盘查找
                let fat_entry: FATDirEntry =
                    d.find_entry(name, None, None, self.fs.upgrade().unwrap())?;
                // kdebug!("find entry from disk ok, entry={fat_entry:?}");
                // 创建新的inode
                let entry_inode: Arc<LockedFATInode> = LockedFATInode::new(
                    self.fs.upgrade().unwrap(),
                    self.self_ref.clone(),
                    fat_entry,
                );
                // 加入缓存区, 由于FAT文件系统的大小写不敏感问题，因此存入缓存区的key应当是全大写的
                self.children
                    .insert(name.to_uppercase(), entry_inode.clone());
                return Ok(entry_inode);
            }
            FATDirEntry::UnInit => {
                panic!(
                    "Uninitialized FAT Inode, fs = {:?}, inode={self:?}",
                    self.fs
                )
            }
            _ => {
                return Err(SystemError::ENOTDIR);
            }
        }
    }
}

impl LockedFATInode {
    pub fn new(
        fs: Arc<FATFileSystem>,
        parent: Weak<LockedFATInode>,
        inode_type: FATDirEntry,
    ) -> Arc<LockedFATInode> {
        let file_type = if let FATDirEntry::Dir(_) = inode_type {
            FileType::Dir
        } else {
            FileType::File
        };

        let inode: Arc<LockedFATInode> = Arc::new(LockedFATInode(SpinLock::new(FATInode {
            parent: parent,
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            fs: Arc::downgrade(&fs),
            inode_type: inode_type,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: fs.bpb.bytes_per_sector as usize,
                blocks: if let FATType::FAT32(_) = fs.bpb.fat_type {
                    fs.bpb.total_sectors_32 as usize
                } else {
                    fs.bpb.total_sectors_16 as usize
                },
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: file_type,
                mode: 0o777,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: 0,
            },
        })));

        inode.0.lock().self_ref = Arc::downgrade(&inode);

        inode.0.lock().update_metadata();

        return inode;
    }
}

/// FsInfo结构体（内存中的一份拷贝，当卸载卷或者sync的时候，把它写入磁盘）
#[derive(Debug)]
pub struct FATFsInfo {
    /// Lead Signature - must equal 0x41615252
    lead_sig: u32,
    /// Value must equal 0x61417272
    struc_sig: u32,
    /// 空闲簇数目
    free_count: u32,
    /// 第一个空闲簇的位置（不一定准确，仅供加速查找）
    next_free: u32,
    /// 0xAA550000
    trail_sig: u32,
    /// Dirty flag to flush to disk
    dirty: bool,
    /// FsInfo Structure 在磁盘上的字节偏移量
    /// Not present for FAT12 and FAT16
    offset: Option<u64>,
}

impl FileSystem for FATFileSystem {
    fn root_inode(&self) -> Arc<dyn crate::filesystem::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> crate::filesystem::vfs::FsInfo {
        todo!()
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

impl FATFileSystem {
    /// FAT12允许的最大簇号
    pub const FAT12_MAX_CLUSTER: u32 = 0xFF5;
    /// FAT16允许的最大簇号
    pub const FAT16_MAX_CLUSTER: u32 = 0xFFF5;
    /// FAT32允许的最大簇号
    pub const FAT32_MAX_CLUSTER: u32 = 0x0FFFFFF7;

    pub fn new(partition: Arc<Partition>) -> Result<Arc<FATFileSystem>, SystemError> {
        let bpb = BiosParameterBlock::new(partition.clone())?;

        // 从磁盘上读取FAT32文件系统的FsInfo结构体
        let fs_info: FATFsInfo = match bpb.fat_type {
            FATType::FAT32(bpb32) => {
                let fs_info_in_disk_bytes_offset = partition.lba_start * LBA_SIZE as u64
                    + bpb32.fs_info as u64 * bpb.bytes_per_sector as u64;
                FATFsInfo::new(
                    partition.clone(),
                    fs_info_in_disk_bytes_offset,
                    bpb.bytes_per_sector as usize,
                )?
            }
            _ => FATFsInfo::default(),
        };

        // 根目录项占用的扇区数（向上取整）
        let root_dir_sectors: u64 = ((bpb.root_entries_cnt as u64 * 32)
            + (bpb.bytes_per_sector as u64 - 1))
            / (bpb.bytes_per_sector as u64);

        // FAT表大小（单位：扇区）
        let fat_size = if bpb.fat_size_16 != 0 {
            bpb.fat_size_16 as u64
        } else {
            match bpb.fat_type {
                FATType::FAT32(x) => x.fat_size_32 as u64,
                _ => {
                    kerror!("FAT12 and FAT16 volumes should have non-zero BPB_FATSz16");
                    return Err(SystemError::EINVAL);
                }
            }
        };

        let first_data_sector =
            bpb.rsvd_sec_cnt as u64 + (bpb.num_fats as u64 * fat_size) + root_dir_sectors;

        // 创建文件系统的根节点
        let root_inode: Arc<LockedFATInode> = Arc::new(LockedFATInode(SpinLock::new(FATInode {
            parent: Weak::default(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            fs: Weak::default(),
            inode_type: FATDirEntry::UnInit,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: bpb.bytes_per_sector as usize,
                blocks: if let FATType::FAT32(_) = bpb.fat_type {
                    bpb.total_sectors_32 as usize
                } else {
                    bpb.total_sectors_16 as usize
                },
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::Dir,
                mode: 0o777,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: 0,
            },
        })));

        let result: Arc<FATFileSystem> = Arc::new(FATFileSystem {
            partition: partition,
            bpb,
            first_data_sector,
            fs_info: Arc::new(LockedFATFsInfo::new(fs_info)),
            root_inode: root_inode,
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<FATInode> = result.root_inode.0.lock();
        root_guard.inode_type = FATDirEntry::Dir(result.root_dir());
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        return Ok(result);
    }

    /// @brief 计算每个簇有多少个字节
    #[inline]
    pub fn bytes_per_cluster(&self) -> u64 {
        return (self.bpb.bytes_per_sector as u64) * (self.bpb.sector_per_cluster as u64);
    }

    /// @brief 读取当前簇在FAT表中存储的信息
    ///
    /// @param cluster 当前簇
    ///
    /// @return Ok(FATEntry) 当前簇在FAT表中，存储的信息。（详情见FATEntry的注释）
    /// @return Err(SystemError) 错误码
    pub fn get_fat_entry(&self, cluster: Cluster) -> Result<FATEntry, SystemError> {
        let current_cluster = cluster.cluster_num;

        let fat_type: FATType = self.bpb.fat_type;
        // 获取FAT表的起始扇区（相对分区起始扇区的偏移量）
        let fat_start_sector = self.fat_start_sector();
        let bytes_per_sec = self.bpb.bytes_per_sector as u64;

        // cluster对应的FAT表项在分区内的字节偏移量
        let fat_bytes_offset =
            fat_type.get_fat_bytes_offset(cluster, fat_start_sector, bytes_per_sec);

        // FAT表项所在的LBA地址
        // let fat_ent_lba = self.get_lba_from_offset(self.bytes_to_sector(fat_bytes_offset));
        let fat_ent_lba = self.partition.lba_start + fat_bytes_offset / LBA_SIZE as u64;

        // FAT表项在逻辑块内的字节偏移量
        let blk_offset = self.get_in_block_offset(fat_bytes_offset);

        let mut v = Vec::<u8>::new();
        v.resize(self.bpb.bytes_per_sector as usize, 0);
        self.partition
            .disk()
            .read_at(fat_ent_lba as usize, 1 * self.lba_per_sector(), &mut v)?;

        let mut cursor = VecCursor::new(v);
        cursor.seek(SeekFrom::SeekSet(blk_offset as i64))?;

        let res: FATEntry = match self.bpb.fat_type {
            FATType::FAT12(_) => {
                let mut entry = cursor.read_u16()?;
                // 由于FAT12文件系统的FAT表，每个entry占用1.5字节，因此奇数的簇需要取高12位的值。
                if (current_cluster & 1) > 0 {
                    entry >>= 4;
                } else {
                    entry &= 0x0fff;
                }

                if entry == 0 {
                    FATEntry::Unused
                } else if entry == 0x0ff7 {
                    FATEntry::Bad
                } else if entry >= 0x0ff8 {
                    FATEntry::EndOfChain
                } else {
                    FATEntry::Next(Cluster {
                        cluster_num: entry as u64,
                        parent_cluster: current_cluster,
                    })
                }
            }
            FATType::FAT16(_) => {
                let entry = cursor.read_u16()?;

                if entry == 0 {
                    FATEntry::Unused
                } else if entry == 0xfff7 {
                    FATEntry::Bad
                } else if entry >= 0xfff8 {
                    FATEntry::EndOfChain
                } else {
                    FATEntry::Next(Cluster {
                        cluster_num: entry as u64,
                        parent_cluster: current_cluster,
                    })
                }
            }
            FATType::FAT32(_) => {
                let entry = cursor.read_u32()? & 0x0fffffff;

                match entry {
                    _n if (current_cluster >= 0x0ffffff7 && current_cluster <= 0x0fffffff) => {
                        // 当前簇号不是一个能被获得的簇（可能是文件系统出错了）
                        kerror!("FAT32 get fat entry: current cluster number [{}] is not an allocatable cluster number.", current_cluster);
                        FATEntry::Bad
                    }
                    0 => FATEntry::Unused,
                    0x0ffffff7 => FATEntry::Bad,
                    0x0ffffff8..=0x0fffffff => FATEntry::EndOfChain,
                    _n => FATEntry::Next(Cluster {
                        cluster_num: entry as u64,
                        parent_cluster: current_cluster,
                    }),
                }
            }
        };
        return Ok(res);
    }

    /// @brief 读取当前簇在FAT表中存储的信息（直接返回读取到的值，而不加处理）
    ///
    /// @param cluster 当前簇
    ///
    /// @return Ok(u64) 当前簇在FAT表中，存储的信息。
    /// @return Err(SystemError) 错误码
    pub fn get_fat_entry_raw(&self, cluster: Cluster) -> Result<u64, SystemError> {
        let current_cluster = cluster.cluster_num;

        let fat_type: FATType = self.bpb.fat_type;
        // 获取FAT表的起始扇区（相对分区起始扇区的偏移量）
        let fat_start_sector = self.fat_start_sector();
        let bytes_per_sec = self.bpb.bytes_per_sector as u64;

        // cluster对应的FAT表项在分区内的字节偏移量
        let fat_bytes_offset =
            fat_type.get_fat_bytes_offset(cluster, fat_start_sector, bytes_per_sec);

        // FAT表项所在的LBA地址
        let fat_ent_lba = self.get_lba_from_offset(self.bytes_to_sector(fat_bytes_offset));

        // FAT表项在逻辑块内的字节偏移量
        let blk_offset = self.get_in_block_offset(fat_bytes_offset);

        let mut v = Vec::<u8>::new();
        v.resize(self.bpb.bytes_per_sector as usize, 0);
        self.partition
            .disk()
            .read_at(fat_ent_lba, 1 * self.lba_per_sector(), &mut v)?;

        let mut cursor = VecCursor::new(v);
        cursor.seek(SeekFrom::SeekSet(blk_offset as i64))?;

        let res = match self.bpb.fat_type {
            FATType::FAT12(_) => {
                let mut entry = cursor.read_u16()?;
                entry = if (current_cluster & 0x0001) > 0 {
                    entry >> 4
                } else {
                    entry & 0x0fff
                };
                entry as u64
            }
            FATType::FAT16(_) => {
                let entry = (cursor.read_u16()?) as u64;
                entry
            }
            FATType::FAT32(_) => {
                let entry = cursor.read_u32()? & 0x0fff_ffff;
                entry as u64
            }
        };

        return Ok(res);
    }

    /// @brief 获取当前文件系统的root inode，在磁盘上的字节偏移量
    pub fn root_dir_bytes_offset(&self) -> u64 {
        match self.bpb.fat_type {
            FATType::FAT32(s) => {
                let first_sec_cluster: u64 = (s.root_cluster as u64 - 2)
                    * (self.bpb.sector_per_cluster as u64)
                    + self.first_data_sector;
                return (self.get_lba_from_offset(first_sec_cluster) * LBA_SIZE) as u64;
            }
            _ => {
                let root_sec = (self.bpb.rsvd_sec_cnt as u64)
                    + (self.bpb.num_fats as u64) * (self.bpb.fat_size_16 as u64);
                return (self.get_lba_from_offset(root_sec) * LBA_SIZE) as u64;
            }
        }
    }

    /// @brief 获取当前文件系统的根目录项区域的结束位置，在磁盘上的字节偏移量。
    /// 请注意，当前函数只对FAT12/FAT16生效。对于FAT32,返回None
    pub fn root_dir_end_bytes_offset(&self) -> Option<u64> {
        match self.bpb.fat_type {
            FATType::FAT12(_) | FATType::FAT16(_) => {
                return Some(
                    self.root_dir_bytes_offset() + (self.bpb.root_entries_cnt as u64) * 32,
                );
            }
            _ => {
                return None;
            }
        }
    }

    /// @brief 获取簇在磁盘内的字节偏移量(相对磁盘起始位置。注意，不是分区内偏移量)
    pub fn cluster_bytes_offset(&self, cluster: Cluster) -> u64 {
        if cluster.cluster_num >= 2 {
            // 指定簇的第一个扇区号
            let first_sec_of_cluster = (cluster.cluster_num - 2)
                * (self.bpb.sector_per_cluster as u64)
                + self.first_data_sector;
            return (self.get_lba_from_offset(first_sec_of_cluster) * LBA_SIZE) as u64;
        } else {
            return 0;
        }
    }

    /// @brief 获取一个空闲簇
    ///
    /// @param prev_cluster 簇链的前一个簇。本函数将会把新获取的簇，连接到它的后面。
    ///
    /// @return Ok(Cluster) 新获取的空闲簇
    /// @return Err(SystemError) 错误码
    pub fn allocate_cluster(&self, prev_cluster: Option<Cluster>) -> Result<Cluster, SystemError> {
        let end_cluster: Cluster = self.max_cluster_number();
        let start_cluster: Cluster = match self.bpb.fat_type {
            FATType::FAT32(_) => {
                let next_free: u64 = match self.fs_info.0.lock().next_free() {
                    Some(x) => x,
                    None => 0xffffffff,
                };
                if next_free < end_cluster.cluster_num {
                    Cluster::new(next_free)
                } else {
                    Cluster::new(RESERVED_CLUSTERS as u64)
                }
            }
            _ => Cluster::new(RESERVED_CLUSTERS as u64),
        };

        // 寻找一个空的簇
        let free_cluster: Cluster = match self.get_free_cluster(start_cluster, end_cluster) {
            Ok(c) => c,
            Err(_) if start_cluster.cluster_num > RESERVED_CLUSTERS as u64 => {
                self.get_free_cluster(Cluster::new(RESERVED_CLUSTERS as u64), end_cluster)?
            }
            Err(e) => return Err(e),
        };

        self.set_entry(free_cluster, FATEntry::EndOfChain)?;
        // 减少空闲簇计数
        self.fs_info.0.lock().update_free_count_delta(-1);
        // 更新搜索空闲簇的参考量
        self.fs_info
            .0
            .lock()
            .update_next_free((free_cluster.cluster_num + 1) as u32);

        // 如果这个空闲簇不是簇链的第一个簇，那么把当前簇跟前一个簇连上。
        if let Some(prev_cluster) = prev_cluster {
            // kdebug!("set entry, prev ={prev_cluster:?}, next = {free_cluster:?}");
            self.set_entry(prev_cluster, FATEntry::Next(free_cluster))?;
        }
        // 清空新获取的这个簇
        self.zero_cluster(free_cluster)?;
        return Ok(free_cluster);
    }

    /// @brief 释放簇链上的所有簇
    ///
    /// @param start_cluster 簇链的第一个簇
    pub fn deallocate_cluster_chain(&self, start_cluster: Cluster) -> Result<(), SystemError> {
        let clusters: Vec<Cluster> = self.clusters(start_cluster);
        for c in clusters {
            self.deallocate_cluster(c)?;
        }
        return Ok(());
    }

    /// @brief 释放簇
    ///
    /// @param 要释放的簇
    pub fn deallocate_cluster(&self, cluster: Cluster) -> Result<(), SystemError> {
        let entry: FATEntry = self.get_fat_entry(cluster)?;
        // 如果不是坏簇
        if entry != FATEntry::Bad {
            self.set_entry(cluster, FATEntry::Unused)?;
            self.fs_info.0.lock().update_free_count_delta(1);
            // 安全选项：清空被释放的簇
            #[cfg(feature = "secure")]
            self.zero_cluster(cluster)?;
            return Ok(());
        } else {
            // 不能释放坏簇
            kerror!("Bad clusters cannot be freed.");
            return Err(SystemError::EFAULT);
        }
    }

    /// @brief 获取文件系统的根目录项
    pub fn root_dir(&self) -> FATDir {
        match self.bpb.fat_type {
            FATType::FAT32(s) => {
                return FATDir {
                    first_cluster: Cluster::new(s.root_cluster as u64),
                    dir_name: String::from("/"),
                    root_offset: None,
                    short_dir_entry: None,
                    loc: None,
                };
            }
            _ => FATDir {
                first_cluster: Cluster::new(0),
                dir_name: String::from("/"),
                root_offset: Some(self.root_dir_bytes_offset()),
                short_dir_entry: None,
                loc: None,
            },
        }
    }

    /// @brief 获取FAT表的起始扇区（相对分区起始扇区的偏移量）
    pub fn fat_start_sector(&self) -> u64 {
        let active_fat = self.active_fat();
        let fat_size = self.fat_size();
        return self.bpb.rsvd_sec_cnt as u64 + active_fat * fat_size;
    }

    /// @brief 获取当前活动的FAT表
    pub fn active_fat(&self) -> u64 {
        if self.mirroring_enabled() {
            return 0;
        } else {
            match self.bpb.fat_type {
                FATType::FAT32(bpb32) => {
                    return (bpb32.ext_flags & 0x0f) as u64;
                }
                _ => {
                    return 0;
                }
            }
        }
    }

    /// @brief 获取当前文件系统的每个FAT表的大小
    pub fn fat_size(&self) -> u64 {
        if self.bpb.fat_size_16 != 0 {
            return self.bpb.fat_size_16 as u64;
        } else {
            match self.bpb.fat_type {
                FATType::FAT32(bpb32) => {
                    return bpb32.fat_size_32 as u64;
                }

                _ => {
                    panic!("FAT12 and FAT16 volumes should have non-zero BPB_FATSz16");
                }
            }
        }
    }

    /// @brief 判断当前文件系统是否启用了FAT表镜像
    pub fn mirroring_enabled(&self) -> bool {
        match self.bpb.fat_type {
            FATType::FAT32(bpb32) => {
                return (bpb32.ext_flags & 0x80) == 0;
            }
            _ => {
                return false;
            }
        }
    }

    /// @brief 根据分区内的扇区偏移量，获得在磁盘上的LBA地址
    #[inline]
    pub fn get_lba_from_offset(&self, in_partition_sec_offset: u64) -> usize {
        return (self.partition.lba_start
            + in_partition_sec_offset * (self.bpb.bytes_per_sector as u64 / LBA_SIZE as u64))
            as usize;
    }

    /// @brief 获取每个扇区占用多少个LBA
    #[inline]
    pub fn lba_per_sector(&self) -> usize {
        return self.bpb.bytes_per_sector as usize / LBA_SIZE;
    }

    /// @brief 将分区内字节偏移量转换为扇区偏移量
    #[inline]
    pub fn bytes_to_sector(&self, in_partition_bytes_offset: u64) -> u64 {
        return in_partition_bytes_offset / (self.bpb.bytes_per_sector as u64);
    }

    /// @brief 根据磁盘上的字节偏移量，获取对应位置在分区内的字节偏移量
    #[inline]
    pub fn get_in_partition_bytes_offset(&self, disk_bytes_offset: u64) -> u64 {
        return disk_bytes_offset - (self.partition.lba_start * LBA_SIZE as u64);
    }

    /// @brief 根据字节偏移量计算在逻辑块内的字节偏移量
    #[inline]
    pub fn get_in_block_offset(&self, bytes_offset: u64) -> u64 {
        return bytes_offset % LBA_SIZE as u64;
    }

    /// @brief 获取在FAT表中，以start_cluster开头的FAT链的所有簇的信息
    ///
    /// @param start_cluster 整个FAT链的起始簇号
    pub fn clusters(&self, start_cluster: Cluster) -> Vec<Cluster> {
        return self.cluster_iter(start_cluster).collect();
    }

    /// @brief 获取在FAT表中，以start_cluster开头的FAT链的长度（总计经过多少个簇）
    ///
    /// @param start_cluster 整个FAT链的起始簇号
    pub fn num_clusters_chain(&self, start_cluster: Cluster) -> u64 {
        return self
            .cluster_iter(start_cluster)
            .fold(0, |size, _cluster| size + 1);
    }
    /// @brief 获取一个簇迭代器对象
    ///
    /// @param start_cluster 整个FAT链的起始簇号
    fn cluster_iter(&self, start_cluster: Cluster) -> ClusterIter {
        return ClusterIter {
            current_cluster: Some(start_cluster),
            fs: self,
        };
    }

    /// @brief 获取从start_cluster开始的簇链中，第n个簇的信息。（请注意，下标从0开始）
    #[inline]
    pub fn get_cluster_by_relative(&self, start_cluster: Cluster, n: usize) -> Option<Cluster> {
        return self.cluster_iter(start_cluster).skip(n).next();
    }

    /// @brief 获取整个簇链的最后一个簇
    #[inline]
    pub fn get_last_cluster(&self, start_cluster: Cluster) -> Option<Cluster> {
        return self.cluster_iter(start_cluster).last();
    }

    /// @brief 判断FAT文件系统的shut bit是否正常。
    /// shut bit 表示文件系统是否正常卸载。如果这一位是1,则表示这个卷是“干净的”
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(true) 正常
    /// @return Ok(false) 不正常
    /// @return Err(SystemError) 在判断时发生错误
    pub fn is_shut_bit_ok(&mut self) -> Result<bool, SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                // 对于FAT32, error bit位于第一个扇区的第8字节。
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x0800_0000;
                return Ok(bit > 0);
            }
            FATType::FAT16(_) => {
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x8000;
                return Ok(bit > 0);
            }
            _ => return Ok(true),
        }
    }

    /// @brief 判断FAT文件系统的hard error bit是否正常。
    /// 如果此位为0，则文件系统驱动程序在上次安装卷时遇到磁盘 I/O 错误，这表明
    /// 卷上的某些扇区可能已损坏。
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(true) 正常
    /// @return Ok(false) 不正常
    /// @return Err(SystemError) 在判断时发生错误
    pub fn is_hard_error_bit_ok(&mut self) -> Result<bool, SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x0400_0000;
                return Ok(bit > 0);
            }
            FATType::FAT16(_) => {
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x4000;
                return Ok(bit > 0);
            }
            _ => return Ok(true),
        }
    }

    /// @brief 设置文件系统的shut bit为正常状态
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(()) 设置成功
    /// @return Err(SystemError) 在设置过程中，出现错误
    pub fn set_shut_bit_ok(&mut self) -> Result<(), SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x0800_0000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;

                return Ok(());
            }

            FATType::FAT16(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x8000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    /// @brief 设置文件系统的hard error bit为正常状态
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(()) 设置成功
    /// @return Err(SystemError) 在设置过程中，出现错误
    pub fn set_hard_error_bit_ok(&mut self) -> Result<(), SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x0400_0000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;
                return Ok(());
            }

            FATType::FAT16(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x4000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    /// @brief 执行文件系统卸载前的一些准备工作：设置好对应的标志位，并把缓存中的数据刷入磁盘
    pub fn umount(&mut self) -> Result<(), SystemError> {
        self.fs_info.0.lock().flush(&self.partition)?;

        self.set_shut_bit_ok()?;

        self.set_hard_error_bit_ok()?;

        self.partition.disk().sync()?;

        return Ok(());
    }

    /// @brief 获取文件系统的最大簇号
    pub fn max_cluster_number(&self) -> Cluster {
        match self.bpb.fat_type {
            FATType::FAT32(s) => {
                // FAT32

                // 数据扇区数量（总扇区数-保留扇区-FAT占用的扇区）
                let data_sec: u64 = self.bpb.total_sectors_32 as u64
                    - (self.bpb.rsvd_sec_cnt as u64
                        + self.bpb.num_fats as u64 * s.fat_size_32 as u64);

                // 数据区的簇数量
                let total_clusters: u64 = data_sec / self.bpb.sector_per_cluster as u64;

                // 返回最大的簇号
                return Cluster::new(total_clusters + RESERVED_CLUSTERS as u64 - 1);
            }

            _ => {
                // FAT12 / FAT16
                let root_dir_sectors: u64 = (((self.bpb.root_entries_cnt as u64) * 32)
                    + self.bpb.bytes_per_sector as u64
                    - 1)
                    / self.bpb.bytes_per_sector as u64;
                // 数据区扇区数
                let data_sec: u64 = self.bpb.total_sectors_16 as u64
                    - (self.bpb.rsvd_sec_cnt as u64
                        + (self.bpb.num_fats as u64 * self.bpb.fat_size_16 as u64)
                        + root_dir_sectors);
                let total_clusters = data_sec / self.bpb.sector_per_cluster as u64;
                return Cluster::new(total_clusters + RESERVED_CLUSTERS as u64 - 1);
            }
        }
    }

    /// @brief 在文件系统中寻找一个簇号在给定的范围（左闭右开区间）内的空闲簇
    ///
    /// @param start_cluster 起始簇号
    /// @param end_cluster 终止簇号（不包含）
    ///
    /// @return Ok(Cluster) 寻找到的空闲簇
    /// @return Err(SystemError) 错误码。如果磁盘无剩余空间，或者簇号达到给定的最大值，则返回-ENOSPC.
    pub fn get_free_cluster(
        &self,
        start_cluster: Cluster,
        end_cluster: Cluster,
    ) -> Result<Cluster, SystemError> {
        let max_cluster: Cluster = self.max_cluster_number();
        let mut cluster: u64 = start_cluster.cluster_num;

        let fat_type: FATType = self.bpb.fat_type;
        let fat_start_sector: u64 = self.fat_start_sector();
        let bytes_per_sec: u64 = self.bpb.bytes_per_sector as u64;

        match fat_type {
            FATType::FAT12(_) => {
                let part_bytes_offset: u64 =
                    fat_type.get_fat_bytes_offset(start_cluster, fat_start_sector, bytes_per_sec);
                let in_block_offset = self.get_in_block_offset(part_bytes_offset);

                let lba = self.get_lba_from_offset(self.bytes_to_sector(part_bytes_offset));

                // 由于FAT12的FAT表不大于6K，因此直接读取6K
                let num_lba = (6 * 1024) / LBA_SIZE;
                let mut v: Vec<u8> = Vec::new();
                v.resize(num_lba * LBA_SIZE, 0);
                self.partition.disk().read_at(lba, num_lba, &mut v)?;

                let mut cursor: VecCursor = VecCursor::new(v);
                cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

                let mut packed_val: u16 = cursor.read_u16()?;
                loop {
                    let val = if (cluster & 0x1) > 0 {
                        packed_val >> 4
                    } else {
                        packed_val & 0x0fff
                    };
                    if val == 0 {
                        return Ok(Cluster::new(cluster as u64));
                    }

                    cluster += 1;

                    // 磁盘无剩余空间，或者簇号达到给定的最大值
                    if cluster == end_cluster.cluster_num || cluster == max_cluster.cluster_num {
                        return Err(SystemError::ENOSPC);
                    }

                    packed_val = match cluster & 1 {
                        0 => cursor.read_u16()?,
                        _ => {
                            let next_byte = cursor.read_u8()? as u16;
                            (packed_val >> 8) | (next_byte << 8)
                        }
                    };
                }
            }
            FATType::FAT16(_) => {
                // todo: 优化这里，减少读取磁盘的次数。
                while cluster < end_cluster.cluster_num && cluster < max_cluster.cluster_num {
                    let part_bytes_offset: u64 = fat_type.get_fat_bytes_offset(
                        Cluster::new(cluster),
                        fat_start_sector,
                        bytes_per_sec,
                    );
                    let in_block_offset = self.get_in_block_offset(part_bytes_offset);

                    let lba = self.get_lba_from_offset(self.bytes_to_sector(part_bytes_offset));

                    let mut v: Vec<u8> = Vec::new();
                    v.resize(self.lba_per_sector() * LBA_SIZE, 0);
                    self.partition
                        .disk()
                        .read_at(lba, self.lba_per_sector(), &mut v)?;

                    let mut cursor: VecCursor = VecCursor::new(v);
                    cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

                    let val = cursor.read_u16()?;
                    // 找到空闲簇
                    if val == 0 {
                        return Ok(Cluster::new(val as u64));
                    }
                    cluster += 1;
                }

                // 磁盘无剩余空间，或者簇号达到给定的最大值
                return Err(SystemError::ENOSPC);
            }
            FATType::FAT32(_) => {
                // todo: 优化这里，减少读取磁盘的次数。
                while cluster < end_cluster.cluster_num && cluster < max_cluster.cluster_num {
                    let part_bytes_offset: u64 = fat_type.get_fat_bytes_offset(
                        Cluster::new(cluster),
                        fat_start_sector,
                        bytes_per_sec,
                    );
                    let in_block_offset = self.get_in_block_offset(part_bytes_offset);

                    let lba = self.get_lba_from_offset(self.bytes_to_sector(part_bytes_offset));

                    let mut v: Vec<u8> = Vec::new();
                    v.resize(self.lba_per_sector() * LBA_SIZE, 0);
                    self.partition
                        .disk()
                        .read_at(lba, self.lba_per_sector(), &mut v)?;

                    let mut cursor: VecCursor = VecCursor::new(v);
                    cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

                    let val = cursor.read_u32()? & 0x0fffffff;

                    if val == 0 {
                        return Ok(Cluster::new(cluster));
                    }
                    cluster += 1;
                }

                // 磁盘无剩余空间，或者簇号达到给定的最大值
                return Err(SystemError::ENOSPC);
            }
        }
    }

    /// @brief 在FAT表中，设置指定的簇的信息。
    ///
    /// @param cluster 目标簇
    /// @param fat_entry 这个簇在FAT表中，存储的信息（下一个簇的簇号）
    pub fn set_entry(&self, cluster: Cluster, fat_entry: FATEntry) -> Result<(), SystemError> {
        // fat表项在分区上的字节偏移量
        let fat_part_bytes_offset: u64 = self.bpb.fat_type.get_fat_bytes_offset(
            cluster,
            self.fat_start_sector(),
            self.bpb.bytes_per_sector as u64,
        );

        match self.bpb.fat_type {
            FATType::FAT12(_) => {
                // 计算要写入的值
                let raw_val: u16 = match fat_entry {
                    FATEntry::Unused => 0,
                    FATEntry::Bad => 0xff7,
                    FATEntry::EndOfChain => 0xfff,
                    FATEntry::Next(c) => c.cluster_num as u16,
                };

                let in_block_offset = self.get_in_block_offset(fat_part_bytes_offset);

                let lba = self.get_lba_from_offset(self.bytes_to_sector(fat_part_bytes_offset));

                let mut v: Vec<u8> = Vec::new();
                v.resize(LBA_SIZE, 0);
                self.partition.disk().read_at(lba, 1, &mut v)?;

                let mut cursor: VecCursor = VecCursor::new(v);
                cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

                let old_val: u16 = cursor.read_u16()?;
                let new_val: u16 = if (cluster.cluster_num & 0x1) > 0 {
                    (old_val & 0x000f) | (raw_val << 4)
                } else {
                    (old_val & 0xf000) | raw_val
                };

                // 写回数据到磁盘上
                cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;
                cursor.write_u16(new_val)?;
                self.partition.disk().write_at(lba, 1, cursor.as_slice())?;
                return Ok(());
            }
            FATType::FAT16(_) => {
                // 计算要写入的值
                let raw_val: u16 = match fat_entry {
                    FATEntry::Unused => 0,
                    FATEntry::Bad => 0xfff7,
                    FATEntry::EndOfChain => 0xfdff,
                    FATEntry::Next(c) => c.cluster_num as u16,
                };

                let in_block_offset = self.get_in_block_offset(fat_part_bytes_offset);

                let lba = self.get_lba_from_offset(self.bytes_to_sector(fat_part_bytes_offset));

                let mut v: Vec<u8> = Vec::new();
                v.resize(LBA_SIZE, 0);
                self.partition.disk().read_at(lba, 1, &mut v)?;

                let mut cursor: VecCursor = VecCursor::new(v);
                cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

                cursor.write_u16(raw_val)?;
                self.partition.disk().write_at(lba, 1, cursor.as_slice())?;

                return Ok(());
            }
            FATType::FAT32(_) => {
                let fat_size: u64 = self.fat_size();
                let bound: u64 = if self.mirroring_enabled() {
                    1
                } else {
                    self.bpb.num_fats as u64
                };
                // kdebug!("set entry, bound={bound}, fat_size={fat_size}");
                for i in 0..bound {
                    // 当前操作的FAT表在磁盘上的字节偏移量
                    let f_offset: u64 = fat_part_bytes_offset + i * fat_size;
                    let in_block_offset: u64 = self.get_in_block_offset(f_offset);
                    let lba = self.get_lba_from_offset(self.bytes_to_sector(f_offset));

                    // kdebug!("set entry, lba={lba}, in_block_offset={in_block_offset}");
                    let mut v: Vec<u8> = Vec::new();
                    v.resize(LBA_SIZE, 0);
                    self.partition.disk().read_at(lba, 1, &mut v)?;

                    let mut cursor: VecCursor = VecCursor::new(v);
                    cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

                    // FAT32的高4位保留
                    let old_bits = cursor.read_u32()? & 0xf0000000;

                    if fat_entry == FATEntry::Unused
                        && cluster.cluster_num >= 0x0ffffff7
                        && cluster.cluster_num <= 0x0fffffff
                    {
                        kerror!(
                            "FAT32: Reserved Cluster {:?} cannot be marked as free",
                            cluster
                        );
                        return Err(SystemError::EPERM);
                    }

                    // 计算要写入的值
                    let mut raw_val: u32 = match fat_entry {
                        FATEntry::Unused => 0,
                        FATEntry::Bad => 0x0FFFFFF7,
                        FATEntry::EndOfChain => 0x0FFFFFFF,
                        FATEntry::Next(c) => c.cluster_num as u32,
                    };

                    // 恢复保留位
                    raw_val |= old_bits;

                    // kdebug!("sent entry, raw_val={raw_val}");

                    cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;
                    cursor.write_u32(raw_val)?;

                    self.partition.disk().write_at(lba, 1, cursor.as_slice())?;
                }

                return Ok(());
            }
        }
    }

    /// @brief 清空指定的簇
    ///
    /// @param cluster 要被清空的簇
    pub fn zero_cluster(&self, cluster: Cluster) -> Result<(), SystemError> {
        // 准备数据，用于写入
        let zeros: Vec<u8> = vec![0u8; self.bytes_per_cluster() as usize];
        let offset: usize = self.cluster_bytes_offset(cluster) as usize;
        self.partition
            .disk()
            .device()
            .write_at(offset, zeros.len(), zeros.as_slice())?;
        return Ok(());
    }
}

impl Drop for FATFileSystem {
    fn drop(&mut self) {
        let r = self.umount();
        if r.is_err() {
            kerror!(
                "Umount FAT filesystem failed: errno={:?}, FS detail:{self:?}",
                r.unwrap_err()
            );
        }
    }
}

impl FATFsInfo {
    const LEAD_SIG: u32 = 0x41615252;
    const STRUC_SIG: u32 = 0x61417272;
    const TRAIL_SIG: u32 = 0xAA550000;
    const FS_INFO_SIZE: u64 = 512;

    /// @brief 从磁盘上读取FAT文件系统的FSInfo结构体
    ///
    /// @param partition 磁盘分区
    /// @param in_disk_fs_info_offset FSInfo扇区在磁盘内的字节偏移量（单位：字节）
    /// @param bytes_per_sec 每扇区字节数
    pub fn new(
        partition: Arc<Partition>,
        in_disk_fs_info_offset: u64,
        bytes_per_sec: usize,
    ) -> Result<Self, SystemError> {
        let mut v = Vec::<u8>::new();
        v.resize(bytes_per_sec, 0);

        // 计算fs_info扇区在磁盘上的字节偏移量，从磁盘读取数据
        partition
            .disk()
            .read_at(in_disk_fs_info_offset as usize / LBA_SIZE, 1, &mut v)?;
        let mut cursor = VecCursor::new(v);

        let mut fsinfo = FATFsInfo::default();

        fsinfo.lead_sig = cursor.read_u32()?;
        cursor.seek(SeekFrom::SeekCurrent(480))?;
        fsinfo.struc_sig = cursor.read_u32()?;
        fsinfo.free_count = cursor.read_u32()?;
        fsinfo.next_free = cursor.read_u32()?;

        cursor.seek(SeekFrom::SeekCurrent(12))?;

        fsinfo.trail_sig = cursor.read_u32()?;
        fsinfo.dirty = false;
        fsinfo.offset = Some(in_disk_fs_info_offset);

        if fsinfo.is_valid() {
            return Ok(fsinfo);
        } else {
            kerror!("Error occurred while parsing FATFsInfo.");
            return Err(SystemError::EINVAL);
        }
    }

    /// @brief 判断是否为正确的FsInfo结构体
    fn is_valid(&self) -> bool {
        self.lead_sig == Self::LEAD_SIG
            && self.struc_sig == Self::STRUC_SIG
            && self.trail_sig == Self::TRAIL_SIG
    }

    /// @brief 根据fsinfo的信息，计算当前总的空闲簇数量
    ///
    /// @param 当前文件系统的最大簇号
    pub fn count_free_cluster(&self, max_cluster: Cluster) -> Option<u64> {
        let count_clusters = max_cluster.cluster_num - RESERVED_CLUSTERS as u64 + 1;
        // 信息不合理，当前的FsInfo中存储的free count大于计算出来的值
        if self.free_count as u64 > count_clusters {
            return None;
        } else {
            match self.free_count {
                // free count字段不可用
                0xffffffff => return None,
                // 返回FsInfo中存储的数据
                n => return Some(n as u64),
            }
        }
    }

    /// @brief 更新FsInfo中的“空闲簇统计信息“为new_count
    ///
    /// 请注意，除非手动调用`flush()`，否则本函数不会将数据刷入磁盘
    pub fn update_free_count_abs(&mut self, new_count: u32) {
        self.free_count = new_count;
    }

    /// @brief 更新FsInfo中的“空闲簇统计信息“，把它加上delta.
    ///
    /// 请注意，除非手动调用`flush()`，否则本函数不会将数据刷入磁盘
    pub fn update_free_count_delta(&mut self, delta: i32) {
        self.free_count = (self.free_count as i32 + delta) as u32;
    }

    /// @brief 更新FsInfo中的“第一个空闲簇统计信息“为next_free.
    ///
    /// 请注意，除非手动调用`flush()`，否则本函数不会将数据刷入磁盘
    pub fn update_next_free(&mut self, next_free: u32) {
        // 这个值是参考量，不一定要准确，仅供加速查找
        self.next_free = next_free;
    }

    /// @brief 获取fs info 记载的第一个空闲簇。（不一定准确，仅供参考）
    pub fn next_free(&self) -> Option<u64> {
        match self.next_free {
            0xffffffff => return None,
            0 | 1 => return None,
            n => return Some(n as u64),
        };
    }

    /// @brief 把fs info刷入磁盘
    ///
    /// @param partition fs info所在的分区
    pub fn flush(&self, partition: &Arc<Partition>) -> Result<(), SystemError> {
        if let Some(off) = self.offset {
            let in_block_offset = off % LBA_SIZE as u64;

            let lba = off as usize / LBA_SIZE;

            let mut v: Vec<u8> = Vec::new();
            v.resize(LBA_SIZE, 0);
            partition.disk().read_at(lba, 1, &mut v)?;

            let mut cursor: VecCursor = VecCursor::new(v);
            cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

            cursor.write_u32(self.lead_sig)?;
            cursor.seek(SeekFrom::SeekCurrent(480))?;
            cursor.write_u32(self.struc_sig)?;
            cursor.write_u32(self.free_count)?;
            cursor.write_u32(self.next_free)?;
            cursor.seek(SeekFrom::SeekCurrent(12))?;
            cursor.write_u32(self.trail_sig)?;

            partition.disk().write_at(lba, 1, cursor.as_slice())?;
        }
        return Ok(());
    }

    /// @brief 读取磁盘上的Fs Info扇区，将里面的内容更新到结构体中
    ///
    /// @param partition fs info所在的分区
    pub fn update(&mut self, partition: Arc<Partition>) -> Result<(), SystemError> {
        if let Some(off) = self.offset {
            let in_block_offset = off % LBA_SIZE as u64;

            let lba = off as usize / LBA_SIZE;

            let mut v: Vec<u8> = Vec::new();
            v.resize(LBA_SIZE, 0);
            partition.disk().read_at(lba, 1, &mut v)?;
            let mut cursor: VecCursor = VecCursor::new(v);
            cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;
            self.lead_sig = cursor.read_u32()?;

            cursor.seek(SeekFrom::SeekCurrent(480))?;
            self.struc_sig = cursor.read_u32()?;
            self.free_count = cursor.read_u32()?;
            self.next_free = cursor.read_u32()?;
            cursor.seek(SeekFrom::SeekCurrent(12))?;
            self.trail_sig = cursor.read_u32()?;
        }
        return Ok(());
    }
}

impl IndexNode for LockedFATInode {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        let mut guard: SpinLockGuard<FATInode> = self.0.lock();
        match &guard.inode_type {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                let r = f.read(
                    &guard.fs.upgrade().unwrap(),
                    &mut buf[0..len],
                    offset as u64,
                );
                guard.update_metadata();
                return r;
            }
            FATDirEntry::Dir(_) => {
                return Err(SystemError::EISDIR);
            }
            FATDirEntry::UnInit => {
                kerror!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        }
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        let mut guard: SpinLockGuard<FATInode> = self.0.lock();
        let fs: &Arc<FATFileSystem> = &guard.fs.upgrade().unwrap();

        match &mut guard.inode_type {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                let r = f.write(fs, &buf[0..len], offset as u64);
                guard.update_metadata();
                return r;
            }
            FATDirEntry::Dir(_) => {
                return Err(SystemError::EISDIR);
            }
            FATDirEntry::UnInit => {
                kerror!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        }
    }

    fn poll(&self) -> Result<PollStatus, SystemError> {
        // 加锁
        let inode: SpinLockGuard<FATInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        return Ok(PollStatus::READ | PollStatus::WRITE);
    }

    fn create(
        &self,
        name: &str,
        file_type: FileType,
        _mode: u32,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 由于FAT32不支持文件权限的功能，因此忽略mode参数

        let mut guard: SpinLockGuard<FATInode> = self.0.lock();
        let fs: &Arc<FATFileSystem> = &guard.fs.upgrade().unwrap();

        match &mut guard.inode_type {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(d) => match file_type {
                FileType::File => {
                    d.create_file(name, fs)?;
                    return Ok(guard.find(name)?);
                }
                FileType::Dir => {
                    d.create_dir(name, fs)?;
                    return Ok(guard.find(name)?);
                }

                FileType::SymLink => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                _ => return Err(SystemError::EINVAL),
            },
            FATDirEntry::UnInit => {
                kerror!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        }
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self;
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let mut guard: SpinLockGuard<FATInode> = self.0.lock();
        let fatent: &FATDirEntry = &guard.inode_type;
        match fatent {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(dir) => {
                // 获取当前目录下的所有目录项
                let mut ret: Vec<String> = Vec::new();
                let dir_iter: FATDirIter = dir.to_iter(guard.fs.upgrade().unwrap());
                for ent in dir_iter {
                    ret.push(ent.name());

                    // ====== 生成inode缓存，存入B树
                    let name: String = ent.name();
                    // kdebug!("name={name}");

                    if guard.children.contains_key(&name.to_uppercase()) == false
                        && name != "."
                        && name != ".."
                    {
                        // 创建新的inode
                        let entry_inode: Arc<LockedFATInode> = LockedFATInode::new(
                            guard.fs.upgrade().unwrap(),
                            guard.self_ref.clone(),
                            ent,
                        );
                        // 加入缓存区, 由于FAT文件系统的大小写不敏感问题，因此存入缓存区的key应当是全大写的
                        guard
                            .children
                            .insert(name.to_uppercase(), entry_inode.clone());
                    }
                }
                return Ok(ret);
            }
            FATDirEntry::UnInit => {
                kerror!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        }
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut guard: SpinLockGuard<FATInode> = self.0.lock();
        let target = guard.find(name)?;
        return Ok(target);
    }

    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut guard: SpinLockGuard<FATInode> = self.0.lock();
        let target: Arc<LockedFATInode> = guard.find(name)?;
        // 对目标inode上锁，以防更改
        let target_guard: SpinLockGuard<FATInode> = target.0.lock();
        // 先从缓存删除
        guard.children.remove(&name.to_uppercase());

        let dir = match &guard.inode_type {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(d) => d,
            FATDirEntry::UnInit => {
                kerror!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        };
        // 检查文件是否存在
        dir.check_existence(name, Some(false), guard.fs.upgrade().unwrap())?;

        // 再从磁盘删除
        let r = dir.remove(guard.fs.upgrade().unwrap().clone(), name, true);
        drop(target_guard);
        return r;
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let mut guard: SpinLockGuard<FATInode> = self.0.lock();
        let target: Arc<LockedFATInode> = guard.find(name)?;
        // 对目标inode上锁，以防更改
        let target_guard: SpinLockGuard<FATInode> = target.0.lock();
        // 先从缓存删除
        guard.children.remove(&name.to_uppercase());

        let dir = match &guard.inode_type {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(d) => d,
            FATDirEntry::UnInit => {
                kerror!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        };
        // 检查文件夹是否存在
        dir.check_existence(name, Some(true), guard.fs.upgrade().unwrap())?;

        // 再从磁盘删除
        let r: Result<(), SystemError> =
            dir.remove(guard.fs.upgrade().unwrap().clone(), name, true);
        if r.is_ok() {
            return r;
        } else {
            let r = r.unwrap_err();
            if r == SystemError::ENOTEMPTY {
                // 如果要删除的是目录，且不为空，则删除动作未发生，重新加入缓存
                guard.children.insert(name.to_uppercase(), target.clone());
                drop(target_guard);
            }
            return Err(r);
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let guard: SpinLockGuard<FATInode> = self.0.lock();
        if guard.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        match ino {
            0 => {
                return Ok(String::from("."));
            }
            1 => {
                return Ok(String::from(".."));
            }
            ino => {
                // 暴力遍历所有的children，判断inode id是否相同
                // TODO: 优化这里，这个地方性能很差！
                let mut key: Vec<String> = guard
                    .children
                    .keys()
                    .filter(|k| guard.children.get(*k).unwrap().metadata().unwrap().inode_id == ino)
                    .cloned()
                    .collect();

                match key.len() {
                    0=>{return Err(SystemError::ENOENT);}
                    1=>{return Ok(key.remove(0));}
                    _ => panic!("FatFS get_entry_name: key.len()={key_len}>1, current inode_id={inode_id}, to find={to_find}", key_len=key.len(), inode_id = guard.metadata.inode_id, to_find=ino)
                }
            }
        }
    }
}

impl Default for FATFsInfo {
    fn default() -> Self {
        return FATFsInfo {
            lead_sig: FATFsInfo::LEAD_SIG,
            struc_sig: FATFsInfo::STRUC_SIG,
            free_count: 0xFFFFFFFF,
            next_free: RESERVED_CLUSTERS,
            trail_sig: FATFsInfo::TRAIL_SIG,
            dirty: false,
            offset: None,
        };
    }
}

impl Cluster {
    pub fn new(cluster: u64) -> Self {
        return Cluster {
            cluster_num: cluster,
            parent_cluster: 0,
        };
    }
}

/// @brief 用于迭代FAT表的内容的簇迭代器对象
#[derive(Debug)]
struct ClusterIter<'a> {
    /// 迭代器的next要返回的簇
    current_cluster: Option<Cluster>,
    /// 属于的文件系统
    fs: &'a FATFileSystem,
}

impl<'a> Iterator for ClusterIter<'a> {
    type Item = Cluster;

    fn next(&mut self) -> Option<Self::Item> {
        // 当前要返回的簇
        let ret: Option<Cluster> = self.current_cluster;

        // 获得下一个要返回簇
        let new: Option<Cluster> = match self.current_cluster {
            Some(c) => {
                let entry: Option<FATEntry> = self.fs.get_fat_entry(c).ok();
                match entry {
                    Some(FATEntry::Next(c)) => Some(c),
                    _ => None,
                }
            }
            _ => None,
        };

        self.current_cluster = new;
        return ret;
    }
}
