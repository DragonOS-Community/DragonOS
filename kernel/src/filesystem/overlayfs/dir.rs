use super::inode::OvlInode;
use super::whiteout::WHITEOUT_DEV;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::{self, FileType, IndexNode};
use alloc::sync::Arc;
use system_error::SystemError;

pub(super) fn mkdir(
    inode: &OvlInode,
    name: &str,
    mode: vfs::InodeMode,
) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();
    create_over_whiteout(
        inode,
        name,
        |dir, temp_name| dir.mkdir(temp_name, mode),
        true,
    )
}

pub(super) fn rmdir(inode: &OvlInode, name: &str) -> Result<(), SystemError> {
    remove(inode, name, true)
}

pub(super) fn unlink(inode: &OvlInode, name: &str) -> Result<(), SystemError> {
    remove(inode, name, false)
}

fn remove(inode: &OvlInode, name: &str, is_dir: bool) -> Result<(), SystemError> {
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();

    let child = inode.lookup_overlay_child(name)?;
    if is_dir && !child.is_dir() {
        return Err(SystemError::ENOTDIR);
    }
    if !is_dir && child.is_dir() {
        return Err(SystemError::EISDIR);
    }

    let lower_positive = inode.lower_positive(name);
    if is_dir && (lower_positive || child.has_lower()) {
        let child_inode: Arc<dyn IndexNode> = child.clone();
        if !is_dir_empty(&child_inode)? {
            return Err(SystemError::ENOTEMPTY);
        }
    }

    let upper_dir = inode.upper_inode.lock().clone();
    if let Some(upper_dir) = upper_dir {
        match upper_dir.find(name) {
            Ok(_) if lower_positive => {
                return inode.replace_upper_with_whiteout_locked(name, is_dir);
            }
            Ok(_) if is_dir => return upper_dir.rmdir(name),
            Ok(_) => return upper_dir.unlink(name),
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }
    }

    if lower_positive {
        inode.create_whiteout_locked(name)
    } else {
        Err(SystemError::ENOENT)
    }
}

pub(super) fn link(
    inode: &OvlInode,
    name: &str,
    other: &Arc<dyn IndexNode>,
) -> Result<(), system_error::SystemError> {
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();

    match inode.find(name) {
        Ok(_) => return Err(SystemError::EEXIST),
        Err(SystemError::ENOENT) => {}
        Err(err) => return Err(err),
    }

    let source = OvlInode::downcast_overlay_inode(other.clone())?;
    let source_fs = source.overlay_fs()?;
    if !Arc::ptr_eq(&fs, &source_fs) {
        return Err(SystemError::EXDEV);
    }

    source.copy_up_locked()?;
    let source_upper = source.upper_inode.lock().clone().ok_or(SystemError::EIO)?;

    create_over_whiteout(
        inode,
        name,
        |dir, temp_name| {
            dir.link(temp_name, &source_upper)?;
            Ok(source_upper.clone())
        },
        false,
    )
    .map(|_| ())
}

pub(super) fn create(
    inode: &OvlInode,
    name: &str,
    file_type: vfs::FileType,
    mode: vfs::InodeMode,
) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();
    create_over_whiteout(
        inode,
        name,
        |dir, temp_name| dir.create(temp_name, file_type, mode),
        file_type == FileType::Dir,
    )
}

pub(super) fn mknod(
    inode: &OvlInode,
    filename: &str,
    mode: vfs::InodeMode,
    dev_t: DeviceNumber,
) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();
    if FileType::from(mode) == FileType::CharDevice && dev_t == WHITEOUT_DEV {
        return Err(SystemError::EPERM);
    }

    create_over_whiteout(
        inode,
        filename,
        |dir, temp_name| dir.mknod(temp_name, mode, dev_t),
        FileType::from(mode) == FileType::Dir,
    )
}

pub(super) fn is_dir_empty(inode: &Arc<dyn IndexNode>) -> Result<bool, SystemError> {
    Ok(inode.list()?.iter().all(|entry| is_dot_entry(entry)))
}

fn create_over_whiteout<F>(
    inode: &OvlInode,
    name: &str,
    create_temp: F,
    is_dir: bool,
) -> Result<Arc<dyn IndexNode>, SystemError>
where
    F: Fn(&Arc<dyn IndexNode>, &str) -> Result<Arc<dyn IndexNode>, SystemError>,
{
    let upper_inode = inode.writable_upper_inode_locked()?;
    match upper_inode.find(name) {
        Ok(found) => {
            if !OvlInode::is_whiteout_inode_checked(&found)? {
                return Err(SystemError::EEXIST);
            }
        }
        Err(SystemError::ENOENT) => return create_temp(&upper_inode, name),
        Err(err) => return Err(err),
    }

    let (workdir, temp_inode, temp_name) = inode.create_workdir_temp(create_temp)?;
    let commit_result = if is_dir {
        workdir.move_to(
            &temp_name,
            &upper_inode,
            name,
            vfs::syscall::RenameFlags::EXCHANGE,
        )
    } else {
        workdir.move_to(
            &temp_name,
            &upper_inode,
            name,
            vfs::syscall::RenameFlags::empty(),
        )
    };

    if let Err(err) = commit_result {
        if let Err(cleanup_err) = OvlInode::cleanup_workdir_temp(&workdir, &temp_name) {
            log::error!(
                "overlayfs: failed to clean workdir temp {temp_name} after publish error {err:?}: {cleanup_err:?}"
            );
        }
        return Err(err);
    }

    if is_dir {
        if let Err(err) = OvlInode::cleanup_workdir_temp(&workdir, &temp_name) {
            log::error!(
                "overlayfs: failed to clean detached workdir temp {temp_name} after publish: {err:?}"
            );
        }
    }

    upper_inode.find(name).or(Ok(temp_inode))
}

fn is_dot_entry(name: &str) -> bool {
    name == "." || name == ".."
}
