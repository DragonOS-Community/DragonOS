use crate::driver::base::block::gendisk::GenDisk;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::ext4::inode::Ext4Inode;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::utils::{user_path_at, DName};
use crate::filesystem::vfs::vcore::{generate_inode_id, try_find_gendisk};
use crate::filesystem::vfs::{
    self, FileSystem, FileSystemMaker, FileSystemMakerData, IndexNode, Magic, MountableFileSystem,
    FSMAKER, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::libs::spinlock::SpinLock;
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::VmFaultReason;
use crate::process::ProcessManager;
use crate::register_mountable_fs;
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use kdepends::another_ext4;
use linkme::distributed_slice;
use system_error::SystemError;

use super::inode::LockedExt4Inode;

pub struct Ext4FileSystem {
    /// 对应 another_ext4 中的实际文件系统
    pub(super) fs: another_ext4::Ext4,
    /// 当前文件系统对应的设备号
    pub(super) raw_dev: DeviceNumber,

    /// 根 inode
    root_inode: Arc<LockedExt4Inode>,
}

impl FileSystem for Ext4FileSystem {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> vfs::FsInfo {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "ext4"
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
        let raw_dev = mount_data.device_num();
        let fs = another_ext4::Ext4::load(mount_data)?;
        let root_inode: Arc<LockedExt4Inode> =
            Arc::new(LockedExt4Inode(SpinLock::new(Ext4Inode {
                inner_inode_num: another_ext4::EXT4_ROOT_INO,
                fs_ptr: Weak::default(),
                page_cache: None,
                children: BTreeMap::new(),
                dname: DName::from("/"),
                vfs_inode_id: generate_inode_id(),
            })));

        let fs = Arc::new(Ext4FileSystem {
            fs,
            raw_dev,
            root_inode,
        });

        let mut guard = fs.root_inode.0.lock();
        guard.fs_ptr = Arc::downgrade(&fs);
        drop(guard);

        Ok(fs)
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
        _raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mount_data = Ext4MountData::from_source(source).map_err(|e| {
            log::error!(
                "Failed to create Ext4 mount data from source '{}': {:?}",
                source,
                e
            );
            e
        })?;
        Ok(Some(Arc::new(mount_data)))
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

impl core::fmt::Debug for Ext4FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ext4")
    }
}
