use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{
    vcore::generate_inode_id, FilePrivateData, FileSystem, FileType, IndexNode, Metadata,
};
use crate::libs::spinlock::SpinLockGuard;
use crate::{libs::spinlock::SpinLock, time::PosixTimeSpec};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;
// use uuid::{uuid, Uuid};
use super::{DevFS, DeviceINode};

#[derive(Debug)]
pub struct ZeroInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedZeroInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
}

#[derive(Debug)]
pub struct LockedZeroInode(SpinLock<ZeroInode>);

impl LockedZeroInode {
    pub fn new() -> Arc<Self> {
        let inode = ZeroInode {
            // uuid: Uuid::new_v5(),
            self_ref: Weak::default(),
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
                file_type: FileType::CharDevice, // 文件夹，block设备，char设备
                mode: ModeType::from_bits_truncate(0o666),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(), // 这里用来作为device number
            },
        };

        let result = Arc::new(LockedZeroInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedZeroInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }
}

impl IndexNode for LockedZeroInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
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

    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写
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

        for itr in buf.iter_mut().take(len) {
            *itr = 0;
        }

        return Ok(len);
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写
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
}
