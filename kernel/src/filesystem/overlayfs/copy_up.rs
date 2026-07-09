use super::inode::OvlInode;
use crate::filesystem::vfs::{
    self,
    file::{File, FileFlags},
    FileType, IndexNode, Metadata,
};
use alloc::string::String;
use alloc::sync::Arc;
use system_error::SystemError;

const COPY_UP_CHUNK_SIZE: usize = 64 * 1024;
type UpperCleanup = Option<(Arc<dyn IndexNode>, String)>;
type CreatedUpper = (Arc<dyn IndexNode>, UpperCleanup);

impl OvlInode {
    pub(super) fn writable_upper_inode(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        if let Some(inode) = self.upper_inode.lock().clone() {
            return Ok(inode);
        }

        self.copy_up()?;
        self.upper_inode.lock().clone().ok_or(SystemError::EROFS)
    }

    pub(super) fn copy_up(&self) -> Result<(), SystemError> {
        let mut upper_inode = self.upper_inode.lock();
        if upper_inode.is_some() {
            return Ok(());
        }

        let lower_inode = self.lower_inodes.first().ok_or(SystemError::ENOENT)?;

        let metadata = lower_inode.metadata()?;
        let (new_upper_inode, cleanup) = self.create_upper_inode(metadata.clone())?;

        let copy_result = (|| -> Result<(), SystemError> {
            if metadata.file_type == FileType::File {
                let size = metadata.size.max(0) as usize;
                let lower_file = File::new(lower_inode.clone(), FileFlags::O_RDONLY)?;
                let upper_file = File::new(new_upper_inode.clone(), FileFlags::O_WRONLY)?;
                let mut buffer = vec![0u8; COPY_UP_CHUNK_SIZE.min(size.max(1))];
                let mut offset = 0usize;

                while offset < size {
                    let chunk_len = (size - offset).min(buffer.len());
                    let read_len = lower_file.pread(offset, chunk_len, &mut buffer[..chunk_len])?;
                    if read_len == 0 {
                        return Err(SystemError::EIO);
                    }

                    let mut written = 0usize;
                    while written < read_len {
                        let n = upper_file.pwrite(
                            offset + written,
                            read_len - written,
                            &buffer[written..read_len],
                        )?;
                        if n == 0 {
                            return Err(SystemError::EIO);
                        }
                        written += n;
                    }
                    offset += read_len;
                }
            }

            Ok(())
        })();

        if let Err(err) = copy_result {
            if let Some((parent, name)) = cleanup {
                let _ = parent.unlink(&name);
            }
            return Err(err);
        }

        *upper_inode = Some(new_upper_inode);

        Ok(())
    }

    fn create_upper_inode(&self, metadata: Metadata) -> Result<CreatedUpper, SystemError> {
        let upper_root_inode = self.upper_root_inode()?;
        if self.redirect.is_empty() {
            return Ok((upper_root_inode, None));
        }

        let (parent_path, name) = match self.redirect.rsplit_once('/') {
            Some((parent_path, name)) => (parent_path, name),
            None => ("", self.redirect.as_str()),
        };

        let parent_inode = self.ensure_upper_dir_path(parent_path)?;
        if let Ok(existing) = parent_inode.find(name) {
            return Ok((existing, None));
        }

        let inode = parent_inode.create_with_data(name, metadata.file_type, metadata.mode, 0)?;
        Ok((inode, Some((parent_inode, name.into()))))
    }

    fn ensure_upper_dir_path(&self, path: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut current = self.upper_root_inode()?;
        if path.is_empty() {
            return Ok(current);
        }

        let mut current_path = String::new();
        for component in path.split('/').filter(|component| !component.is_empty()) {
            if !current_path.is_empty() {
                current_path.push('/');
            }
            current_path.push_str(component);

            match current.find(component) {
                Ok(next) => current = next,
                Err(SystemError::ENOENT) => {
                    let mode = self.lower_dir_mode(&current_path)?;
                    current = current.mkdir(component, mode)?;
                }
                Err(err) => return Err(err),
            }
        }

        Ok(current)
    }

    fn lower_dir_mode(&self, path: &str) -> Result<vfs::InodeMode, SystemError> {
        let fs = self.overlay_fs()?;
        for layer in fs.layers.iter().skip(1) {
            if let Some(lower_root) = layer.mnt.lower_inodes.first() {
                if let Ok(inode) = lower_root.lookup(path) {
                    return Ok(inode.metadata()?.mode);
                }
            }
        }

        Ok(vfs::InodeMode::S_IRWXUGO)
    }
}
