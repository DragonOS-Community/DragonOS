//! Common, unmounted pseudo filesystem for anonymous file descriptors.
//!
//! The descriptor's own `IndexNode` implements its operations and owns its
//! state.  This module only supplies the shared filesystem identity and the
//! metadata/name conventions; fd installation remains with each syscall.

use alloc::{string::String, sync::Arc};
use core::any::Any;

use crate::{
    arch::MMArch,
    filesystem::vfs::{
        file::FilePrivateData, vcore::generate_inode_id, FileSystem, FileType, FsInfo, IndexNode,
        InodeMode, Magic, Metadata, SuperBlock,
    },
    libs::mutex::MutexGuard,
    mm::MemoryManagementArch,
};
use system_error::SystemError;

lazy_static::lazy_static! {
    static ref ANON_INODE_FS: Arc<AnonInodeFs> = Arc::new(AnonInodeFs);
    static ref ANON_INODE_ID: crate::filesystem::vfs::InodeId = generate_inode_id();
}

#[derive(Debug)]
pub struct AnonInodeFs;

impl AnonInodeFs {
    pub fn instance() -> Arc<dyn FileSystem> {
        ANON_INODE_FS.clone()
    }
}

impl FileSystem for AnonInodeFs {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // The pseudo filesystem is never mounted.  The inert root satisfies
        // FileSystem's contract without creating a live descriptor object.
        Arc::new(AnonInodeRoot)
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
        "anon_inodefs"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(Magic::ANON_INODEFS_MAGIC, MMArch::PAGE_SIZE as u64, 255)
    }
}

#[derive(Debug)]
struct AnonInodeRoot;

impl IndexNode for AnonInodeRoot {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        AnonInodeFs::instance()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(anon_inode_metadata(InodeMode::S_IRUSR | InodeMode::S_IWUSR))
    }

    fn stat_mode(&self, metadata: &Metadata) -> InodeMode {
        metadata.mode
    }
}

/// A descriptor has no directory entry, but still needs stable per-object
/// metadata for `fstat`.  Linux's anon_inode mode has permission bits without
/// a regular-file type bit; `file_type` remains an internal VFS classification.
pub fn anon_inode_metadata(mode: InodeMode) -> Metadata {
    Metadata {
        inode_id: *ANON_INODE_ID,
        file_type: FileType::File,
        mode,
        ..Default::default()
    }
}

pub fn anon_inode_path(name: &str) -> String {
    // Linux passes the provider's raw dentry name. Some names deliberately
    // include brackets, while inotify and BPF names do not.
    alloc::format!("anon_inode:{name}")
}
