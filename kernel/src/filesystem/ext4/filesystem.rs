use crate::driver::base::block::gendisk::GenDisk;
use crate::filesystem::vfs::{
    self, FileSystem, FileSystemMaker, FileSystemMakerData, IndexNode, Magic, FSMAKER,
};
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
}

impl Ext4FileSystem {
    pub fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<Arc<GenDisk>>())
            .ok_or(SystemError::EINVAL)?;

        Self::from_gendisk(mount_data.clone())
    }

    pub fn from_gendisk(mount_data: Arc<GenDisk>) -> Result<Arc<dyn FileSystem>, SystemError> {
        let fs = another_ext4::Ext4::load(mount_data)?;
        Ok(Arc::new_cyclic(|me| Ext4FileSystem {
            fs,
            self_ref: me.clone(),
        }))
    }
}

#[distributed_slice(FSMAKER)]
static EXT4MAKER: FileSystemMaker = FileSystemMaker::new(
    "ext4",
    &(Ext4FileSystem::make_fs
        as fn(
            Option<&dyn FileSystemMakerData>,
        ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
);

impl core::fmt::Debug for Ext4FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ext4")
    }
}
