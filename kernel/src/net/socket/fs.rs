//! Filesystem identity for anonymous socket inodes.
//!
//! Linux places every socket inode on the single, never-mounted `sockfs`
//! superblock. The inode itself owns socket state and operations; this module
//! only supplies the stable filesystem identity used by `fstatfs(2)` and the
//! mandatory `FileSystem` contract.

use alloc::sync::Arc;
use core::any::Any;

use crate::{
    arch::MMArch,
    filesystem::vfs::{
        file::FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, InodeMode, Magic, Metadata,
        SuperBlock,
    },
    libs::mutex::MutexGuard,
    mm::MemoryManagementArch,
};
use system_error::SystemError;

lazy_static::lazy_static! {
    static ref SOCK_FS: Arc<SockFs> = Arc::new(SockFs);
    static ref SOCK_FS_ROOT: Arc<SockFsRoot> = Arc::new(SockFsRoot);
}

/// Statfs identity shared by every socket inode.
#[derive(Debug)]
pub struct SockFs;

impl SockFs {
    pub fn instance() -> Arc<dyn FileSystem> {
        SOCK_FS.clone()
    }
}

impl FileSystem for SockFs {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // sockfs is never mounted. The inert root only satisfies the
        // FileSystem contract without creating a live descriptor object.
        SOCK_FS_ROOT.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "sockfs"
    }

    fn super_block(&self) -> SuperBlock {
        socket_super_block()
    }
}

/// Build the Linux-compatible statfs view of sockfs.
pub(crate) fn socket_super_block() -> SuperBlock {
    let mut sb = SuperBlock::new(Magic::SOCKFS_MAGIC, MMArch::PAGE_SIZE as u64, 255);
    // Linux statfs_by_dentry() fills an unset fragment size from bsize.
    sb.frsize = sb.bsize;
    sb
}

#[derive(Debug)]
struct SockFsRoot;

impl IndexNode for SockFsRoot {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        Err(SystemError::EINVAL.into())
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        Err(SystemError::EINVAL.into())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        SockFs::instance()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(Metadata::new(FileType::Dir, InodeMode::S_IRWXUGO))
    }
}
