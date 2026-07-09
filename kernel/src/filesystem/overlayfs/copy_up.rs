use super::inode::OvlInode;
use crate::filesystem::vfs::{
    self,
    file::{File, FileFlags, FilePrivateData},
    syscall::RenameFlags,
    FileType, IndexNode, Metadata,
};
use crate::libs::mutex::Mutex;
use alloc::string::String;
use alloc::sync::Arc;
use system_error::SystemError;

const COPY_UP_CHUNK_SIZE: usize = 64 * 1024;

impl OvlInode {
    pub(super) fn writable_upper_inode_locked(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        if let Some(inode) = self.upper_inode.lock().clone() {
            return Ok(inode);
        }

        self.copy_up_locked()?;
        self.upper_inode.lock().clone().ok_or(SystemError::EROFS)
    }

    pub(super) fn copy_up_for_open(&self, flags: &FileFlags) -> Result<(), SystemError> {
        let copy_size = if flags.contains(FileFlags::O_TRUNC) {
            Some(0)
        } else {
            None
        };

        let fs = self.overlay_fs()?;
        let _mutation_guard = fs.mutation_lock.lock();
        self.copy_up_locked_with_size(copy_size)
    }

    pub(super) fn copy_up_locked(&self) -> Result<(), SystemError> {
        self.copy_up_locked_with_size(None)
    }

    fn copy_up_locked_with_size(&self, copy_size: Option<usize>) -> Result<(), SystemError> {
        let mut upper_inode = self.upper_inode.lock();
        if upper_inode.is_some() {
            return Ok(());
        }

        let lower_inode = self.lower_inodes.first().ok_or(SystemError::ENOENT)?;

        let metadata = lower_inode.metadata()?;
        if self.redirect.is_empty() {
            *upper_inode = Some(self.upper_root_inode()?);
            return Ok(());
        }

        let (parent_path, name) = self.upper_parent_path_and_name();
        let parent_inode = self.ensure_upper_dir_path(parent_path)?;
        match parent_inode.find(name) {
            Ok(existing) => {
                *upper_inode = Some(Self::validate_existing_upper(existing, &metadata)?);
                return Ok(());
            }
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }

        let symlink_target = if metadata.file_type == FileType::SymLink {
            Some(Self::read_symlink_target(lower_inode.clone(), &metadata)?)
        } else {
            None
        };

        let (workdir, temp_inode, temp_name) = self.create_workdir_temp(|workdir, temp_name| {
            Self::create_copy_up_temp(workdir, temp_name, &metadata, symlink_target.as_deref())
        })?;

        if let Err(err) = Self::copy_data_if_needed(
            lower_inode.clone(),
            temp_inode.clone(),
            &metadata,
            copy_size,
        ) {
            Self::cleanup_workdir_temp(&workdir, &temp_name);
            return Err(err);
        }

        match workdir.move_to(&temp_name, &parent_inode, name, RenameFlags::NOREPLACE) {
            Ok(()) => {
                *upper_inode = Some(Self::validate_existing_upper(temp_inode, &metadata)?);
                return Ok(());
            }
            Err(SystemError::EEXIST) => {
                Self::cleanup_workdir_temp(&workdir, &temp_name);
                let existing = parent_inode.find(name)?;
                *upper_inode = Some(Self::validate_existing_upper(existing, &metadata)?);
                return Ok(());
            }
            Err(err) => {
                Self::cleanup_workdir_temp(&workdir, &temp_name);
                return Err(err);
            }
        }
    }

    fn upper_parent_path_and_name(&self) -> (&str, &str) {
        match self.redirect.rsplit_once('/') {
            Some((parent_path, name)) => (parent_path, name),
            None => ("", self.redirect.as_str()),
        }
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

    fn validate_existing_upper(
        inode: Arc<dyn IndexNode>,
        lower_metadata: &Metadata,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if Self::is_whiteout_inode(&inode) {
            return Err(SystemError::ENOENT);
        }

        let upper_metadata = inode.metadata()?;
        if upper_metadata.file_type != lower_metadata.file_type {
            return Err(SystemError::EIO);
        }

        if matches!(
            upper_metadata.file_type,
            FileType::CharDevice | FileType::BlockDevice
        ) && upper_metadata.raw_dev != lower_metadata.raw_dev
        {
            return Err(SystemError::EIO);
        }

        Ok(inode)
    }

    fn create_copy_up_temp(
        workdir: &Arc<dyn IndexNode>,
        temp_name: &str,
        metadata: &Metadata,
        symlink_target: Option<&str>,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        match metadata.file_type {
            FileType::SymLink => {
                workdir.symlink(temp_name, symlink_target.ok_or(SystemError::EIO)?)
            }
            FileType::CharDevice | FileType::BlockDevice | FileType::Pipe | FileType::Socket => {
                let mode = (metadata.mode & !vfs::InodeMode::S_IFMT)
                    | vfs::InodeMode::from(metadata.file_type);
                workdir.mknod(temp_name, mode, metadata.raw_dev)
            }
            _ => workdir.create_with_data(temp_name, metadata.file_type, metadata.mode, 0),
        }
    }

    fn copy_data_if_needed(
        lower_inode: Arc<dyn IndexNode>,
        upper_inode: Arc<dyn IndexNode>,
        metadata: &Metadata,
        copy_size: Option<usize>,
    ) -> Result<(), SystemError> {
        if metadata.file_type != FileType::File {
            return Ok(());
        }

        let size = copy_size.unwrap_or_else(|| metadata.size.max(0) as usize);
        if size == 0 {
            return Ok(());
        }

        let lower_file = File::new(lower_inode, FileFlags::O_RDONLY)?;
        let upper_file = File::new(upper_inode, FileFlags::O_WRONLY)?;
        let mut buffer = vec![0u8; COPY_UP_CHUNK_SIZE.min(size)];
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

        Ok(())
    }

    fn read_symlink_target(
        lower_inode: Arc<dyn IndexNode>,
        metadata: &Metadata,
    ) -> Result<String, SystemError> {
        let size = metadata.size.max(0) as usize;
        let mut buffer = vec![0u8; size];
        let mut offset = 0usize;

        while offset < size {
            let read_len = lower_inode.read_at(
                offset,
                size - offset,
                &mut buffer[offset..],
                Mutex::new(FilePrivateData::Unused).lock(),
            )?;
            if read_len == 0 {
                return Err(SystemError::EIO);
            }
            offset += read_len;
        }

        String::from_utf8(buffer).map_err(|_| SystemError::EINVAL)
    }
}
