use super::OvlInode;
use crate::{
    filesystem::vfs::{IndexNode, Metadata},
    libs::spinlock::SpinLock,
};
use alloc::sync::Arc;
use system_error::SystemError;

impl OvlInode {
    pub fn copy_up(&self) -> Result<(), SystemError> {
        let mut upper_inode = self.upper_inode.lock();
        if upper_inode.is_some() {
            return Ok(());
        }

        let lower_inode = self.lower_inode.as_ref().ok_or(SystemError::ENOENT)?;

        let metadata = lower_inode.metadata()?;
        let new_upper_inode = self.create_upper_inode(metadata.clone())?;

        let mut buffer = vec![0u8; metadata.size as usize];
        let lock = SpinLock::new(crate::filesystem::vfs::FilePrivateData::Unused);
        lower_inode.read_at(0, metadata.size as usize, &mut buffer, lock.lock())?;

        new_upper_inode.write_at(0, metadata.size as usize, &buffer, lock.lock())?;

        *upper_inode = Some(new_upper_inode);

        Ok(())
    }

    fn create_upper_inode(&self, metadata: Metadata) -> Result<Arc<dyn IndexNode>, SystemError> {
        let upper_inode = self.upper_inode.lock();
        let upper_root_inode = upper_inode
            .as_ref()
            .ok_or(SystemError::ENOSYS)?
            .fs()
            .root_inode();
        upper_root_inode.create_with_data(&self.dname()?.0, metadata.file_type, metadata.mode, 0)
    }
}
