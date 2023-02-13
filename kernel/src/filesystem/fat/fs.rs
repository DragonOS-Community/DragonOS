use core::{fmt::Debug, any::Any};

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
};

use crate::{
    filesystem::vfs::{FileSystem, FileType, IndexNode, Metadata, PollStatus, file::FilePrivateData},
    include::bindings::bindings::EISDIR,
    libs::spinlock::SpinLock, io::{device::BlockDevice, disk_info::Partition},
};

use super::{
    bpb::{BiosParameterBlock, FATType},
    entry::{FATDirEntry, FATEntry, FATRawDirEntry, ShortDirEntry},
    utils::RESERVED_CLUSTERS,
};

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
    pub disk: Arc<Partition>,
    pub bpb: BiosParameterBlock,
    pub partition_offset: u64,
    pub first_data_sector: u64,
    pub fs_info: Arc<FATFsInfo>,
    /// 文件系统的根inode
    root_inode: Arc<LockedFATInode>,
}

/// FAT文件系统的Inode
#[derive(Debug)]
pub struct LockedFATInode(SpinLock<FATInode>);

#[derive(Debug)]
pub struct FATInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedFATInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedFATInode>,
    /// 子Inode的B树
    children: BTreeMap<String, Arc<LockedFATInode>>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<FATFileSystem>,

    /// 根据不同的Inode类型，创建不同的私有字段
    inode_type: FATDirEntry,
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
    /// Relative Offset of FsInfo Structure
    /// Not present for FAT12 and FAT16
    offset: Option<u64>,
}

impl FileSystem for FATFileSystem {
    fn get_root_inode(&self) -> Arc<dyn crate::filesystem::vfs::IndexNode> {
        todo!()
    }

    fn info(&self) -> crate::filesystem::vfs::FsInfo {
        todo!()
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any{
        self
    }
}

impl FATFileSystem {
    pub fn new(
        partition_offset: u64,
        disk: Arc<Partition>,
    ) -> Result<Arc<FATFileSystem>, i32> {
        todo!()
    }

    /// @brief 计算每个簇有多少个字节
    pub fn bytes_per_cluster(&self) -> u64 {
        return (self.bpb.bytes_per_sector as u64) * (self.bpb.sector_per_cluster as u64);
    }

    /// @brief 读取当前簇在FAT表中存储的信息
    ///
    /// @param current_cluster 当前簇
    ///
    /// @return Ok(FATEntry) 当前簇在FAT表中，存储的信息。（详情见FATEntry的注释）
    /// @return Err(i32) 错误码
    pub fn get_fat_entry(&self, current_cluster: Cluster) -> Result<FATEntry, i32> {
        todo!()
    }

    /// @brief 获取当前文件系统的root inode，在磁盘上的字节偏移量
    pub fn root_dir_bytes_offset(&self) -> u64 {
        match self.bpb.fat_type {
            FATType::FAT32(s) => {
                let first_sec_cluster: u64 = (s.root_cluster as u64 - 2)
                    * (self.bpb.sector_per_cluster as u64)
                    + self.first_data_sector;
                return first_sec_cluster * (self.bpb.bytes_per_sector as u64);
            }
            _ => {
                let root_sec = (self.bpb.rsvd_sec_cnt as u64)
                    + (self.bpb.num_fats as u64) * (self.bpb.fat_size_16 as u64);
                return root_sec * (self.bpb.bytes_per_sector as u64);
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

    /// @brief 获取簇在磁盘内的字节偏移量
    pub fn cluster_bytes_offset(&self, cluster: Cluster) -> u64 {
        if cluster.cluster_num >= 2 {
            let first_sec_of_cluster = (cluster.cluster_num - 2)
                * (self.bpb.sector_per_cluster as u64)
                + self.first_data_sector;
            return (self.bpb.bytes_per_sector as u64) * first_sec_of_cluster;
        } else {
            return 0;
        }
    }

    /// @brief 根据字节偏移量，读取磁盘，并生成一个FATRawDirEntry对象
    pub fn get_raw_dir_entry(&self, bytes_offset: u64) -> Result<FATRawDirEntry, i32> {
        todo!()
    }
}

impl FATFsInfo {
    const LEAD_SIG: u32 = 0x41615252;
    const STRUC_SIG: u32 = 0x61417272;
    const TRAIL_SIG: u32 = 0xAA550000;
    const FS_INFO_SIZE: u64 = 512;

    /// @brief 判断是否为正确的FsInfo结构体
    fn is_valid(&self) -> bool {
        self.lead_sig == Self::LEAD_SIG
            && self.struc_sig == Self::STRUC_SIG
            && self.trail_sig == Self::TRAIL_SIG
    }
}

impl IndexNode for LockedFATInode {
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8], _data: &mut FilePrivateData) -> Result<usize, i32> {
        todo!()
    }

    fn write_at(&self, offset: usize, len: usize, buf: &mut [u8], _data: &mut FilePrivateData) -> Result<usize, i32> {
        todo!()
    }

    fn poll(&self) -> Result<PollStatus, i32> {
        // 加锁
        let inode = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        return Ok(PollStatus {
            flags: PollStatus::READ_MASK | PollStatus::WRITE_MASK,
        });
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self;
    }

    fn metadata(&self) -> Result<Metadata, i32> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn list(&self) -> Result<alloc::vec::Vec<String>, i32> {
        todo!()
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
