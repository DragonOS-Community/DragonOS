use super::OvlInode;
use crate::{
    filesystem::vfs::{FileType, IndexNode, Metadata},
    libs::mutex::Mutex,
};
use alloc::sync::Arc;
use system_error::SystemError;

impl OvlInode {
    pub fn copy_up(&self) -> Result<(), SystemError> {
        let mut upper_inode = self.upper_inode.lock();
        if upper_inode.is_some() {
            return Ok(());
        }

        let lower_inode = self.lower_inodes.first().ok_or(SystemError::ENOENT)?;

        let metadata = lower_inode.metadata()?;
        let new_upper_inode = self.create_upper_inode(metadata.clone())?;

        if metadata.file_type == FileType::File {
            let mut buffer = vec![0u8; metadata.size as usize];
            let lock = Mutex::new(crate::filesystem::vfs::FilePrivateData::Unused);
            lower_inode.read_at(0, metadata.size as usize, &mut buffer, lock.lock())?;

            new_upper_inode.write_at(0, metadata.size as usize, &buffer, lock.lock())?;
        }

        *upper_inode = Some(new_upper_inode);

        Ok(())
    }

    fn create_upper_inode(&self, metadata: Metadata) -> Result<Arc<dyn IndexNode>, SystemError> {
        let upper_root_inode = self.upper_root_inode()?;
        if self.redirect.is_empty() {
            return Ok(upper_root_inode);
        }

        let (parent_path, name) = match self.redirect.rsplit_once('/') {
            Some((parent_path, name)) => (parent_path, name),
            None => ("", self.redirect.as_str()),
        };

        let parent_inode = self.ensure_upper_dir_path(parent_path)?;
        if let Ok(existing) = parent_inode.find(name) {
            return Ok(existing);
        }

        parent_inode.create_with_data(name, metadata.file_type, metadata.mode, 0)
    }
}
