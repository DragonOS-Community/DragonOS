use super::inode::OvlInode;
use crate::driver::base::device::device_number::{DeviceNumber, Major};
use crate::filesystem::vfs::{self, FileType, IndexNode};
use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

pub(super) const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0);

impl OvlInode {
    pub(super) fn is_whiteout_inode_checked(
        inode: &Arc<dyn IndexNode>,
    ) -> Result<bool, SystemError> {
        let metadata = inode.metadata()?;
        Ok(metadata.file_type == FileType::CharDevice && metadata.raw_dev == WHITEOUT_DEV)
    }

    pub(super) fn is_whiteout_inode(inode: &Arc<dyn IndexNode>) -> bool {
        Self::is_whiteout_inode_checked(inode).unwrap_or(false)
    }

    pub(super) fn create_whiteout_locked(&self, name: &str) -> Result<(), SystemError> {
        let whiteout_mode = vfs::InodeMode::S_IFCHR;
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV)?;
            return Ok(());
        }

        self.copy_up_locked()?;
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV)?;
            return Ok(());
        }
        Err(SystemError::EROFS)
    }

    pub(super) fn replace_upper_with_whiteout_locked(
        &self,
        name: &str,
        is_dir: bool,
    ) -> Result<(), SystemError> {
        let upper_dir = self.writable_upper_inode_locked()?;
        let upper_entry = upper_dir.find(name)?;
        let internal_whiteouts = if is_dir {
            Some(Self::validated_whiteout_entries(&upper_entry)?)
        } else {
            None
        };

        let whiteout_mode = vfs::InodeMode::S_IFCHR;
        let (workdir, _, temp_name) = self.create_workdir_temp(|workdir, temp_name| {
            workdir.mknod(temp_name, whiteout_mode, WHITEOUT_DEV)
        })?;
        let flags = if is_dir {
            vfs::syscall::RenameFlags::EXCHANGE
        } else {
            vfs::syscall::RenameFlags::empty()
        };

        if let Err(err) = workdir.move_to(&temp_name, &upper_dir, name, flags) {
            let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
            return Err(err);
        }

        if let Some(entries) = internal_whiteouts {
            Self::cleanup_detached_whiteout_dir(&workdir, &temp_name, &entries);
        }
        Ok(())
    }

    fn validated_whiteout_entries(dir: &Arc<dyn IndexNode>) -> Result<Vec<String>, SystemError> {
        let mut whiteouts = Vec::new();
        for name in dir.list()? {
            if name == "." || name == ".." {
                continue;
            }
            let entry = dir.find(&name)?;
            if !Self::is_whiteout_inode_checked(&entry)? {
                return Err(SystemError::ENOTEMPTY);
            }
            whiteouts.push(name);
        }
        Ok(whiteouts)
    }

    fn cleanup_detached_whiteout_dir(
        workdir: &Arc<dyn IndexNode>,
        temp_name: &str,
        entries: &[String],
    ) {
        let detached = match workdir.find(temp_name) {
            Ok(detached) => detached,
            Err(err) => {
                log::error!(
                    "overlayfs: failed to find detached upper directory {temp_name}: {err:?}"
                );
                return;
            }
        };

        for name in entries {
            let still_whiteout = detached
                .find(name)
                .and_then(|entry| Self::is_whiteout_inode_checked(&entry));
            match still_whiteout {
                Ok(true) => {
                    if let Err(err) = detached.unlink(name) {
                        log::error!(
                            "overlayfs: failed to remove detached whiteout {temp_name}/{name}: {err:?}"
                        );
                    }
                }
                Ok(false) => log::error!(
                    "overlayfs: detached entry {temp_name}/{name} is no longer a whiteout"
                ),
                Err(err) => log::error!(
                    "overlayfs: failed to revalidate detached whiteout {temp_name}/{name}: {err:?}"
                ),
            }
        }

        if let Err(err) = workdir.rmdir(temp_name) {
            log::error!(
                "overlayfs: failed to remove detached upper directory {temp_name}: {err:?}"
            );
        }
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
