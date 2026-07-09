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
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();
    if let Some(ref upper_inode) = *inode.upper_inode.lock() {
        match upper_inode.rmdir(name) {
            Ok(()) => return Ok(()),
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }
    }

    match inode.find(name) {
        Ok(found) => {
            if found.metadata()?.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }
            if !is_dir_empty(&found)? {
                return Err(SystemError::ENOTEMPTY);
            }
            return inode.create_whiteout(name);
        }
        Err(SystemError::ENOENT) => {}
        Err(err) => return Err(err),
    }

    Err(SystemError::ENOENT)
}

pub(super) fn unlink(inode: &OvlInode, name: &str) -> Result<(), SystemError> {
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();
    if let Some(ref upper_inode) = *inode.upper_inode.lock() {
        match upper_inode.unlink(name) {
            Ok(()) => return Ok(()),
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }
    }

    match inode.find(name) {
        Ok(found) => {
            if found.metadata()?.file_type == FileType::Dir {
                return Err(SystemError::EISDIR);
            }
            return inode.create_whiteout(name);
        }
        Err(SystemError::ENOENT) => {}
        Err(err) => return Err(err),
    }

    Err(SystemError::ENOENT)
}

pub(super) fn link(
    inode: &OvlInode,
    name: &str,
    other: &Arc<dyn IndexNode>,
) -> Result<(), system_error::SystemError> {
    let fs = inode.overlay_fs()?;
    let _mutation_guard = fs.mutation_lock.lock();
    create_over_whiteout(
        inode,
        name,
        |dir, temp_name| {
            dir.link(temp_name, other)?;
            dir.find(temp_name)
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
    let upper_inode = inode.writable_upper_inode()?;
    match upper_inode.find(name) {
        Ok(found) if OvlInode::is_whiteout_inode(&found) => {}
        Ok(_) => return Err(SystemError::EEXIST),
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
        OvlInode::cleanup_workdir_temp(&workdir, &temp_name);
        return Err(err);
    }

    if is_dir {
        OvlInode::cleanup_workdir_temp(&workdir, &temp_name);
    }

    upper_inode.find(name).or(Ok(temp_inode))
}

fn is_dot_entry(name: &str) -> bool {
    name == "." || name == ".."
}
