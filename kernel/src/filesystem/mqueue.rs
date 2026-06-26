use core::any::Any;

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::utils::DName;
use crate::filesystem::vfs::{
    file::FilePrivateData, vcore::generate_inode_id, FileSystem, FileSystemMakerData, FileType,
    FsInfo, IndexNode, InodeFlags, InodeId, InodeMode, Magic, Metadata, MountableFileSystem,
    SuperBlock, FSMAKER,
};
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::register_mountable_fs;
use crate::time::PosixTimeSpec;

use linkme::distributed_slice;

const MQUEUE_MAX_NAMELEN: u64 = 255;
const MQUEUE_BLOCK_SIZE: u64 = 4096;

#[derive(Debug)]
pub struct MqueueFs {
    root: Arc<MqueueRootInode>,
    super_block: SuperBlock,
}

#[derive(Debug)]
pub struct MqueueRootInode {
    self_ref: Weak<MqueueRootInode>,
    fs: Mutex<Weak<MqueueFs>>,
    metadata: Metadata,
}

impl MqueueFs {
    fn new() -> Arc<Self> {
        let super_block =
            SuperBlock::new(Magic::MQUEUE_MAGIC, MQUEUE_BLOCK_SIZE, MQUEUE_MAX_NAMELEN);

        Arc::new_cyclic(|weak_fs| {
            let root = Arc::new_cyclic(|weak_root| MqueueRootInode {
                self_ref: weak_root.clone(),
                fs: Mutex::new(weak_fs.clone()),
                metadata: Metadata {
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    size: 0,
                    blk_size: MQUEUE_BLOCK_SIZE as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    file_type: FileType::Dir,
                    mode: InodeMode::S_IFDIR | InodeMode::S_ISVTX | InodeMode::S_IRWXUGO,
                    nlinks: 2,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                    flags: InodeFlags::empty(),
                },
            });

            Self { root, super_block }
        })
    }
}

impl FileSystem for MqueueFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: MQUEUE_MAX_NAMELEN as usize,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "mqueue"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.clone()
    }
}

impl MountableFileSystem for MqueueFs {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        Ok(None)
    }

    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        Ok(Self::new())
    }
}

register_mountable_fs!(MqueueFs, MQUEUEFSMAKER, "mqueue");

impl IndexNode for MqueueRootInode {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        match name {
            "" | "." | ".." => Ok(self.self_ref.upgrade().ok_or(SystemError::ENOENT)?),
            _ => Err(SystemError::ENOENT),
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        if ino == self.metadata.inode_id {
            return Ok(String::from("."));
        }
        Err(SystemError::ENOENT)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Ok(vec![String::from("."), String::from("..")])
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.lock().upgrade().unwrap()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(DName::from("mqueue"))
    }

    fn create(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        Err(SystemError::ENOSYS)
    }
}
