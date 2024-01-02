use crate::driver::base::block::block_device::BlockDevice;
use crate::driver::base::device::device_number::{DeviceNumber, Major};
use crate::filesystem::devfs::{DevFS, DeviceINode};
use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{
    core::generate_inode_id, FilePrivateData, FileSystem, FileType, IndexNode, Metadata,
};
use crate::{libs::spinlock::SpinLock, time::TimeSpec};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::ahcidisk::LockedAhciDisk;

#[derive(Debug)]
pub struct AhciInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedAhciInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
    /// INode 对应的磁盘
    disk: Arc<LockedAhciDisk>,
}

#[derive(Debug)]
pub struct LockedAhciInode(pub SpinLock<AhciInode>);

impl LockedAhciInode {
    pub fn new(disk: Arc<LockedAhciDisk>) -> Arc<Self> {
        let inode = AhciInode {
            // uuid: Uuid::new_v5(),
            self_ref: Weak::default(),
            fs: Weak::default(),
            disk: disk,
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::BlockDevice, // 文件夹，block设备，char设备
                mode: ModeType::from_bits_truncate(0o666),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::new(Major::HD_MAJOR, 0),
            },
        };

        let result = Arc::new(LockedAhciInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedAhciInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }
}

impl IndexNode for LockedAhciInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn read_at(
        &self,
        offset: usize, // lba地址
        len: usize,
        buf: &mut [u8],
        data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        if let FilePrivateData::Unused = data {
            return self.0.lock().disk.read_at_bytes(offset, len, buf);
        }

        return Err(SystemError::EINVAL);
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn write_at(
        &self,
        offset: usize, // lba地址
        len: usize,
        buf: &[u8],
        data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        if let FilePrivateData::Unused = data {
            return self.0.lock().disk.write_at_bytes(offset, len, buf);
        }

        return Err(SystemError::EINVAL);
    }
}
