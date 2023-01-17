use core::fmt::Debug;

use alloc::sync::Arc;

use crate::filesystem::vfs::FileSystem;

use super::bpb::BiosParameterBlock;

/// 临时定义的结构体，待block device抽象merge后，删掉这个
pub trait BlockDevice: Debug + Sync + Send {}

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
    pub disk: Arc<dyn BlockDevice>,
    pub bpb: BiosParameterBlock,
    pub partition_offset: u64,
    pub first_data_sector: u64,
    pub fs_info: Arc<FATFsInfo>,
}

#[derive(Debug)]
pub struct FATFsInfo {}

impl FileSystem for FATFileSystem {
    fn get_root_inode(&self) -> Arc<dyn crate::filesystem::vfs::IndexNode> {
        todo!()
    }

    fn info(&self) -> crate::filesystem::vfs::FsInfo {
        todo!()
    }
}

impl FATFileSystem {
    pub fn new(partition_offset: u64, disk: Arc<dyn BlockDevice>) -> Result<FATFileSystem, i32> {
        todo!()
    }
}
