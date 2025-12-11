use crate::driver::base::device::device_number::{DeviceNumber, Major};
use crate::filesystem::devfs::LockedDevFSInode;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::{
    vcore::generate_inode_id, FilePrivateData, FileSystem, FileType, IndexNode, InodeFlags,
    InodeMode, Metadata,
};
use crate::libs::rand::rand_bytes;
use crate::libs::spinlock::SpinLockGuard;
use crate::{filesystem::devfs::DevFS, libs::spinlock::SpinLock, time::PosixTimeSpec};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{cmp::min, mem::size_of};
use system_error::SystemError;

use super::DeviceINode;

#[derive(Debug)]
pub struct RandomInode {
    self_ref: Weak<LockedRandomInode>,
    fs: Weak<DevFS>,
    parent: Weak<LockedDevFSInode>,
    metadata: Metadata,
}

#[derive(Debug)]
pub struct LockedRandomInode(SpinLock<RandomInode>);

impl LockedRandomInode {
    pub fn new() -> Arc<Self> {
        let inode = RandomInode {
            self_ref: Weak::default(),
            fs: Weak::default(),
            parent: Weak::default(),
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::CharDevice,
                mode: InodeMode::from_bits_truncate(0o666),
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::new(Major::new(1), 8),
            },
        };

        let result = Arc::new(LockedRandomInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);
        result
    }
}

impl DeviceINode for LockedRandomInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }

    fn set_parent(&self, parent: Weak<LockedDevFSInode>) {
        self.0.lock().parent = parent;
    }
}

impl IndexNode for LockedRandomInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.0.lock().metadata.clone())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.0.lock().fs.upgrade().unwrap()
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.btime = metadata.btime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;
        Ok(())
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Err(SystemError::ENODEV)
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        let mut copied = 0;
        while copied < len {
            let chunk = rand_bytes::<{ size_of::<usize>() }>();
            let copy_len = min(len - copied, chunk.len());
            buf[copied..copied + copy_len].copy_from_slice(&chunk[..copy_len]);
            copied += copy_len;
        }

        Ok(len)
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        Ok(len)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.0.lock().parent.upgrade();
        if let Some(parent) = parent {
            return Ok(parent);
        }
        Err(SystemError::ENOENT)
    }
}
