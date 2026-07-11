use super::inode::OvlInode;
use super::{cred::CredOverrideGuard, metadata};
use crate::filesystem::vfs::{
    self,
    file::{File, FileFlags, FilePrivateData},
    syscall::RenameFlags,
    utils::should_remove_sgid,
    FileType, IndexNode, Metadata, SetMetadataMask, MAX_PATHLEN,
};
use crate::libs::mutex::Mutex;
use crate::process::{
    cred::{CAPFlags, Cred},
    ProcessManager,
};
use alloc::string::String;
use alloc::sync::Arc;
use system_error::SystemError;

const COPY_UP_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyUpOutcome {
    Published,
    PublishedAfterTruncate,
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenCopyUpOutcome {
    NoTruncateRequested,
    TruncateCompletedBeforePublish,
    NeedsPostOpenTruncate,
}

impl OpenCopyUpOutcome {
    pub(super) fn needs_post_open_truncate(self) -> bool {
        self == Self::NeedsPostOpenTruncate
    }
}

impl OvlInode {
    pub(super) fn writable_upper_inode_locked(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        if let Some(inode) = self.upper_inode.lock().clone() {
            return Ok(inode);
        }

        self.copy_up_locked()?;
        self.upper_inode.lock().clone().ok_or(SystemError::EROFS)
    }

    pub(super) fn copy_up_for_open(
        &self,
        flags: &FileFlags,
    ) -> Result<OpenCopyUpOutcome, SystemError> {
        let copy_size = if flags.contains(FileFlags::O_TRUNC) {
            Some(0)
        } else {
            None
        };

        let fs = self.overlay_fs()?;
        let _mutation_guard = fs.mutation_lock.lock();
        let outcome = self.copy_up_locked_with_size(copy_size)?;
        if copy_size.is_none() {
            Ok(OpenCopyUpOutcome::NoTruncateRequested)
        } else if outcome == CopyUpOutcome::PublishedAfterTruncate {
            Ok(OpenCopyUpOutcome::TruncateCompletedBeforePublish)
        } else {
            Ok(OpenCopyUpOutcome::NeedsPostOpenTruncate)
        }
    }

    pub(super) fn copy_up_locked(&self) -> Result<(), SystemError> {
        self.copy_up_locked_with_size(None).map(|_| ())
    }

    pub(super) fn copy_up_locked_for_truncate(&self, len: usize) -> Result<(), SystemError> {
        self.copy_up_locked_with_size(Some(len)).map(|_| ())
    }

    fn copy_up_locked_with_size(
        &self,
        copy_size: Option<usize>,
    ) -> Result<CopyUpOutcome, SystemError> {
        let fs = self.overlay_fs()?;
        let caller_cred = ProcessManager::current_pcb().cred();
        let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
        let mut upper_inode = self.upper_inode.lock();
        if upper_inode.is_some() {
            return Ok(CopyUpOutcome::Existing);
        }

        let lower_inode = self.lower_inodes.first().ok_or(SystemError::ENOENT)?;

        let mut metadata = lower_inode.metadata()?;
        Self::adjust_metadata_for_truncate_copy_up(&mut metadata, copy_size, &caller_cred);
        if self.redirect.is_empty() {
            *upper_inode = Some(self.upper_root_inode()?);
            return Ok(CopyUpOutcome::Existing);
        }

        let (parent_path, name) = self.upper_parent_path_and_name();
        let parent_inode = self.ensure_upper_dir_path(parent_path)?;
        match parent_inode.find(name) {
            Ok(existing) => {
                let existing = Self::validate_existing_upper(existing, &metadata)?;
                self.set_origin(metadata::load_origin(self, &existing)?);
                *upper_inode = Some(existing);
                return Ok(CopyUpOutcome::Existing);
            }
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }

        let symlink_target = if metadata.file_type == FileType::SymLink {
            Some(Self::read_symlink_target(lower_inode.clone())?)
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
            let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
            return Err(err);
        }

        if let Err(err) = metadata::copy_xattrs(lower_inode, &temp_inode) {
            let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
            return Err(err);
        }

        // A length-aware copy-up already contains the post-truncate data.
        // Drop capabilities before publishing that changed content so lookup
        // and exec can never observe a stale capability on the new upper.
        if copy_size.is_some() && metadata.file_type == FileType::File {
            if let Err(err) = metadata::remove_security_capability(&temp_inode) {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
                return Err(err);
            }
        }

        let origin = match metadata::prepare_origin(self, lower_inode, &temp_inode, &metadata) {
            Ok(origin) => origin,
            Err(err) => {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
                return Err(err);
            }
        };

        if let Err(err) = Self::restore_copy_up_metadata(&temp_inode, &metadata) {
            let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
            return Err(err);
        }

        let publish_outcome = if copy_size == Some(0) && metadata.file_type == FileType::File {
            if let Err(err) = vfs::vcore::vfs_truncate(temp_inode.clone(), 0) {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
                return Err(err);
            }
            CopyUpOutcome::PublishedAfterTruncate
        } else {
            CopyUpOutcome::Published
        };

        let parent_metadata = match parent_inode.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
                return Err(err);
            }
        };
        match workdir.move_to(&temp_name, &parent_inode, name, RenameFlags::NOREPLACE) {
            Ok(()) => {
                Self::restore_parent_timestamps(&parent_inode, &parent_metadata);
                self.set_origin(origin);
                *upper_inode = Some(Self::validate_existing_upper(temp_inode, &metadata)?);
                return Ok(publish_outcome);
            }
            Err(SystemError::EEXIST) => {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
                let existing = parent_inode.find(name)?;
                let existing = Self::validate_existing_upper(existing, &metadata)?;
                self.set_origin(metadata::load_origin(self, &existing)?);
                *upper_inode = Some(existing);
                return Ok(CopyUpOutcome::Existing);
            }
            Err(err) => {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
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
                Ok(next) => {
                    if Self::is_whiteout_inode_checked(&next)?
                        || next.metadata()?.file_type != FileType::Dir
                    {
                        return Err(SystemError::ENOTDIR);
                    }
                    current = next;
                }
                Err(SystemError::ENOENT) => {
                    current = self.copy_up_dir_component(&current, component, &current_path)?;
                }
                Err(err) => return Err(err),
            }
        }

        Ok(current)
    }

    fn lower_dir_inode(&self, path: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let fs = self.overlay_fs()?;
        for layer in fs.layers.iter().skip(1) {
            if let Some(lower_root) = layer.mnt.lower_inodes.first() {
                if let Ok(inode) = lower_root.lookup(path) {
                    if inode.metadata()?.file_type == FileType::Dir {
                        return Ok(inode);
                    }
                    return Err(SystemError::ENOTDIR);
                }
            }
        }
        Err(SystemError::ENOENT)
    }

    fn copy_up_dir_component(
        &self,
        upper_parent: &Arc<dyn IndexNode>,
        name: &str,
        lower_path: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let lower = self.lower_dir_inode(lower_path)?;
        let lower_metadata = lower.metadata()?;
        let parent_metadata = upper_parent.metadata()?;
        let (workdir, temp, temp_name) = self.create_workdir_temp(|workdir, temp_name| {
            workdir.mkdir(temp_name, lower_metadata.mode)
        })?;

        let prepared = (|| {
            metadata::copy_xattrs(&lower, &temp)?;
            metadata::prepare_origin(self, &lower, &temp, &lower_metadata)?;
            Self::restore_copy_up_metadata(&temp, &lower_metadata)
        })();
        if let Err(err) = prepared {
            let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
            return Err(err);
        }

        match workdir.move_to(&temp_name, upper_parent, name, RenameFlags::NOREPLACE) {
            Ok(()) => {
                Self::restore_parent_timestamps(upper_parent, &parent_metadata);
                Ok(temp)
            }
            Err(SystemError::EEXIST) => {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
                let existing = upper_parent.find(name)?;
                if Self::is_whiteout_inode_checked(&existing)?
                    || existing.metadata()?.file_type != FileType::Dir
                {
                    return Err(SystemError::EIO);
                }
                Ok(existing)
            }
            Err(err) => {
                let _ = Self::cleanup_workdir_temp(&workdir, &temp_name);
                Err(err)
            }
        }
    }

    fn validate_existing_upper(
        inode: Arc<dyn IndexNode>,
        lower_metadata: &Metadata,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if Self::is_whiteout_inode(&inode) {
            return Err(SystemError::ENOENT);
        }

        let upper_metadata = inode.metadata()?;
        if Self::copy_up_file_type(upper_metadata.file_type)
            != Self::copy_up_file_type(lower_metadata.file_type)
        {
            return Err(SystemError::EIO);
        }

        if Self::is_device_node_file_type(lower_metadata.file_type)
            && upper_metadata.raw_dev != lower_metadata.raw_dev
        {
            return Err(SystemError::EIO);
        }

        Ok(inode)
    }

    fn adjust_metadata_for_truncate_copy_up(
        metadata: &mut Metadata,
        copy_size: Option<usize>,
        caller_cred: &Arc<Cred>,
    ) {
        if copy_size.is_none() || metadata.file_type != FileType::File {
            return;
        }

        Self::clear_suid_sgid_for_current_cred(metadata, caller_cred);
    }

    fn clear_suid_sgid_for_current_cred(metadata: &mut Metadata, cred: &Arc<Cred>) {
        if cred.has_capability(CAPFlags::CAP_FSETID) {
            return;
        }

        if !metadata
            .mode
            .intersects(vfs::InodeMode::S_ISUID | vfs::InodeMode::S_ISGID)
        {
            return;
        }

        metadata.mode.remove(vfs::InodeMode::S_ISUID);

        if should_remove_sgid(metadata.mode, metadata.gid, cred) {
            metadata.mode.remove(vfs::InodeMode::S_ISGID);
        }
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
            file_type if Self::is_mknod_copy_up_type(file_type) => {
                let mode = (metadata.mode & !vfs::InodeMode::S_IFMT)
                    | vfs::InodeMode::from(Self::copy_up_file_type(file_type));
                workdir.mknod(temp_name, mode, metadata.raw_dev)
            }
            _ => workdir.create_with_data(temp_name, metadata.file_type, metadata.mode, 0),
        }
    }

    fn restore_copy_up_metadata(
        upper_inode: &Arc<dyn IndexNode>,
        lower_metadata: &Metadata,
    ) -> Result<(), SystemError> {
        let mut upper_metadata = upper_inode.metadata()?;
        if lower_metadata.file_type != FileType::SymLink {
            upper_metadata.mode = lower_metadata.mode;
        }
        upper_metadata.uid = lower_metadata.uid;
        upper_metadata.gid = lower_metadata.gid;
        upper_metadata.atime = lower_metadata.atime;
        upper_metadata.mtime = lower_metadata.mtime;
        upper_inode.set_metadata(&upper_metadata)
    }

    fn restore_parent_timestamps(parent: &Arc<dyn IndexNode>, saved: &Metadata) {
        let result = (|| {
            let mut current = parent.metadata()?;
            current.atime = saved.atime;
            current.mtime = saved.mtime;
            parent.set_metadata_masked(&current, SetMetadataMask::ATIME | SetMetadataMask::MTIME)
        })();
        if let Err(err) = result {
            log::warn!("overlayfs: failed to restore parent timestamps: {err:?}");
        }
    }

    fn copy_up_file_type(file_type: FileType) -> FileType {
        match file_type {
            FileType::KvmDevice | FileType::FramebufferDevice => FileType::CharDevice,
            _ => file_type,
        }
    }

    fn is_device_node_file_type(file_type: FileType) -> bool {
        matches!(
            file_type,
            FileType::CharDevice
                | FileType::BlockDevice
                | FileType::KvmDevice
                | FileType::FramebufferDevice
        )
    }

    fn is_mknod_copy_up_type(file_type: FileType) -> bool {
        Self::is_device_node_file_type(file_type)
            || matches!(file_type, FileType::Pipe | FileType::Socket)
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

        let lower_size = metadata.size.max(0) as usize;
        let size = copy_size.map_or(lower_size, |target_size| target_size.min(lower_size));
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

    fn read_symlink_target(lower_inode: Arc<dyn IndexNode>) -> Result<String, SystemError> {
        let mut buffer = vec![0u8; MAX_PATHLEN];
        let len = lower_inode.read_at(
            0,
            MAX_PATHLEN,
            &mut buffer,
            Mutex::new(FilePrivateData::Unused).lock(),
        )?;

        if len == 0 {
            return Err(SystemError::EIO);
        }
        if len >= MAX_PATHLEN {
            return Err(SystemError::ENAMETOOLONG);
        }

        buffer.truncate(len);
        String::from_utf8(buffer).map_err(|_| SystemError::EINVAL)
    }
}
