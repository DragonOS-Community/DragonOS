use crate::filesystem::vfs::FSMAKER;
use crate::{
    driver::base::block::gendisk::GenDisk,
    filesystem::vfs::{
        self, FileSystem, FileSystemMakerData, MountableFileSystem, VFS_MAX_FOLLOW_SYMLINK_TIMES,
        fcntl::AtFlags, utils::user_path_at, vcore::try_find_gendisk,
    },
    process::ProcessManager,
    register_mountable_fs,
};
use alloc::sync::Arc;
use system_error::SystemError;

use crate::filesystem::vfs::FileSystemMaker;
use linkme::distributed_slice;

use super::fs::FATFileSystem;

pub struct FatMountData {
    gendisk: Arc<GenDisk>,
}

impl FileSystemMakerData for FatMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl FatMountData {
    fn from_source(path: &str) -> Result<Self, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), path)?;
        let inode = current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        if !inode.metadata()?.file_type.eq(&vfs::FileType::BlockDevice) {
            return Err(SystemError::ENOTBLK);
        }

        let disk = inode.dname()?;

        if let Some(gendisk) = try_find_gendisk(disk.0.as_str()) {
            return Ok(Self { gendisk });
        }
        Err(SystemError::ENOENT)
    }
}

impl MountableFileSystem for FATFileSystem {
    fn make_fs(
        data: Option<&dyn crate::filesystem::vfs::FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<FatMountData>())
            .ok_or(SystemError::EINVAL)?;

        let fs = Self::new(mount_data.gendisk.clone())?;
        Ok(fs)
    }
    fn make_mount_data(
        _raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mount_data = FatMountData::from_source(source).map_err(|e| {
            log::error!(
                "Failed to create FAT mount data from source '{}': {:?}",
                source,
                e
            );
            e
        })?;
        Ok(Some(Arc::new(mount_data)))
    }
}

register_mountable_fs!(FATFileSystem, FATFSMAKER, "vfat");
