use super::{DevFS, DeviceINode, LockedDevFSInode};
use crate::{
    driver::base::device::device_number::{DeviceNumber, Major},
    filesystem::vfs::{
        file::FileFlags, utils::DName, vcore::generate_inode_id, FilePrivateData, FileSystem,
        FileType, IndexNode, InodeFlags, InodeMode, Metadata,
    },
    libs::mutex::{Mutex, MutexGuard},
    time::PosixTimeSpec,
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

#[derive(Debug)]
pub struct FullInode {
    self_ref: Weak<LockedFullInode>,
    fs: Weak<DevFS>,
    parent: Weak<LockedDevFSInode>,
    metadata: Metadata,
}

#[derive(Debug)]
pub struct LockedFullInode(Mutex<FullInode>);

impl LockedFullInode {
    pub fn new() -> Arc<Self> {
        let inode = FullInode {
            self_ref: Weak::default(),
            parent: Weak::default(),
            fs: Weak::default(),
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
                raw_dev: DeviceNumber::new(Major::new(1), 7),
            },
        };

        let result = Arc::new(LockedFullInode(Mutex::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedFullInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }

    fn set_parent(&self, parent: Weak<LockedDevFSInode>) {
        self.0.lock().parent = parent;
    }
}

impl IndexNode for LockedFullInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
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

        return Ok(());
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inode.metadata, now, relatime);
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        for itr in buf.iter_mut().take(len) {
            *itr = 0;
        }

        return Ok(len);
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSPC)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.0.lock().parent.upgrade();
        if let Some(parent) = parent {
            return Ok(parent as Arc<dyn IndexNode>);
        }
        Err(SystemError::ENOENT)
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(DName::from("full"))
    }
}
