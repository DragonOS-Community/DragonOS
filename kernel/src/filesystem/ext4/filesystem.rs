use crate::driver::base::block::gendisk::GenDisk;
use crate::filesystem::vfs::vcore::try_find_gendisk;
use crate::filesystem::vfs::{
    self, FileSystem, FileSystemMaker, FileSystemMakerData, IndexNode, Magic, MountableFileSystem,
    FSMAKER,
};
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::VmFaultReason;
use crate::register_mountable_fs;
use alloc::sync::{Arc, Weak};
use linkme::distributed_slice;
use system_error::SystemError;

pub struct Ext4FileSystem {
    pub(super) fs: another_ext4::Ext4,
    self_ref: Weak<Self>,
}

impl FileSystem for Ext4FileSystem {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        super::inode::Ext4Inode::point_to_root(self.self_ref.clone())
    }

    fn info(&self) -> vfs::FsInfo {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "Ext4"
    }

    fn super_block(&self) -> vfs::SuperBlock {
        vfs::SuperBlock::new(Magic::EXT4_MAGIC, another_ext4::BLOCK_SIZE as u64, 255)
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        PageFaultHandler::filemap_fault(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start_pgoff, end_pgoff)
    }
}

impl Ext4FileSystem {
    pub fn from_gendisk(mount_data: Arc<GenDisk>) -> Result<Arc<dyn FileSystem>, SystemError> {
        let fs = another_ext4::Ext4::load(mount_data)?;
        Ok(Arc::new_cyclic(|me| Ext4FileSystem {
            fs,
            self_ref: me.clone(),
        }))
    }
}

impl MountableFileSystem for Ext4FileSystem {
    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<Ext4MountData>())
            .ok_or(SystemError::EINVAL)?;

        Self::from_gendisk(mount_data.gendisk.clone())
    }
    fn make_mount_data(
        _raw_data: *const u8,
        source: &str,
    ) -> Result<Arc<dyn FileSystemMakerData + 'static>, SystemError> {
        let mount_data = Ext4MountData::form_source(source)?;
        Ok(Arc::new(mount_data))
    }
}

register_mountable_fs!(Ext4FileSystem, EXT4FSMAKER, "ext4");

pub struct Ext4MountData {
    gendisk: Arc<GenDisk>,
}

impl FileSystemMakerData for Ext4MountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Ext4MountData {
    pub fn form_source(path: &str) -> Result<Self, SystemError> {
        //todo 进一步的检查
        if !path.starts_with('/') {
            return Err(SystemError::EINVAL);
        }
        if let Some(gendisk) = try_find_gendisk(path) {
            return Ok(Self { gendisk });
        }
        Err(SystemError::ENOENT)
    }
}

impl core::fmt::Debug for Ext4FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ext4")
    }
}
