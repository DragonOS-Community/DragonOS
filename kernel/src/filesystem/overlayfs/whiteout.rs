use super::inode::OvlInode;
use crate::driver::base::device::device_number::{DeviceNumber, Major};
use crate::filesystem::vfs::{self, FileType, IndexNode};
use alloc::sync::Arc;
use system_error::SystemError;

pub(super) const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0);

impl OvlInode {
    pub(super) fn is_whiteout_inode(inode: &Arc<dyn IndexNode>) -> bool {
        inode
            .metadata()
            .map(|metadata| {
                metadata.file_type == FileType::CharDevice && metadata.raw_dev == WHITEOUT_DEV
            })
            .unwrap_or(false)
    }

    pub(super) fn create_whiteout(&self, name: &str) -> Result<(), SystemError> {
        let whiteout_mode = vfs::InodeMode::S_IFCHR;
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV)?;
            return Ok(());
        }

        self.copy_up()?;
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV)?;
            return Ok(());
        }
        Err(SystemError::EROFS)
    }

    #[allow(dead_code)]
    pub(super) fn is_whiteout(&self) -> bool {
        self.file_type == FileType::CharDevice
            && self
                .metadata()
                .map(|metadata| metadata.raw_dev == WHITEOUT_DEV)
                .unwrap_or(false)
    }

    pub(super) fn has_whiteout(&self, name: &str) -> bool {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            if let Ok(inode) = upper_inode.find(name) {
                return Self::is_whiteout_inode(&inode);
            }
        }
        false
    }

    #[allow(dead_code)]
    pub(super) fn remove_whiteout_if_present(&self, name: &str) -> Result<bool, SystemError> {
        let upper_inode = self.upper_inode.lock().clone().ok_or(SystemError::EROFS)?;
        match upper_inode.find(name) {
            Ok(inode) => {
                if Self::is_whiteout_inode(&inode) {
                    upper_inode.unlink(name)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(SystemError::ENOENT) => Ok(false),
            Err(err) => Err(err),
        }
    }
}
