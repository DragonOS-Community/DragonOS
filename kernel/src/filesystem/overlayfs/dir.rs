use super::inode::{DirState, OvlInode};
use super::whiteout::WHITEOUT_DEV;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::{self, mount::DentryMutationContext, FileType, IndexNode};
use alloc::sync::Arc;
use system_error::SystemError;

pub(super) fn mkdir(
    inode: &OvlInode,
    name: &str,
    mode: vfs::InodeMode,
) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
    let state = inode.dir_state()?;
    let _mutation_guard = state.mutation_lock.lock();
    let result = create_over_whiteout(
        inode,
        name,
        |dir, temp_name| dir.mkdir(temp_name, mode),
        true,
    );
    if result.is_ok() {
        state.modified(&[name]);
    }
    result
}

pub(super) fn rmdir(inode: &OvlInode, name: &str) -> Result<(), SystemError> {
    remove(inode, name, true, None)
}

pub(super) fn rmdir_with_context(
    inode: &OvlInode,
    name: &str,
    context: &DentryMutationContext<'_>,
) -> Result<(), SystemError> {
    remove(inode, name, true, Some(context))
}

pub(super) fn unlink(inode: &OvlInode, name: &str) -> Result<(), SystemError> {
    remove(inode, name, false, None)
}

pub(super) fn unlink_with_context(
    inode: &OvlInode,
    name: &str,
    context: &DentryMutationContext<'_>,
) -> Result<(), SystemError> {
    remove(inode, name, false, Some(context))
}

fn remove(
    inode: &OvlInode,
    name: &str,
    is_dir: bool,
    context: Option<&DentryMutationContext<'_>>,
) -> Result<(), SystemError> {
    let fs = inode.overlay_fs()?;
    // rmdir keeps the mount-wide commit lock because it nests a child directory
    // emptiness check. Unlink only needs the stable parent namespace lock.
    let _commit_guard = is_dir.then(|| fs.mutation_lock.lock());
    let state = inode.dir_state()?;
    let _mutation_guard = state.mutation_lock.lock();

    let child = inode.lookup_overlay_child_locked(name, &state)?;
    if is_dir && !child.is_dir() {
        return Err(SystemError::ENOTDIR);
    }
    if !is_dir && child.is_dir() {
        return Err(SystemError::EISDIR);
    }

    // Serialize the emptiness check and namespace commit with mutations made
    // through an already-resolved fd for the child directory.  The parent
    // lock alone cannot exclude create/unlink operations inside that child.
    let child_state = is_dir.then(|| child.dir_state()).transpose()?;
    let _child_mutation_guard = child_state.as_ref().map(|state| state.mutation_lock.lock());

    let lower_positive = inode.lower_positive(name);
    if is_dir
        && (lower_positive || child.has_lower())
        && !is_dir_empty_locked(&child, child_state.as_ref().ok_or(SystemError::EIO)?)?
    {
        return Err(SystemError::ENOTEMPTY);
    }

    let upper_dir = inode.upper_inode.lock().clone();
    let result = if let Some(upper_dir) = upper_dir {
        match upper_dir.find(name) {
            Ok(_) if lower_positive => {
                inode.replace_upper_with_whiteout_locked(name, is_dir, context)
            }
            Ok(_) if is_dir => rmdir_backing(&upper_dir, name, context),
            Ok(_) => unlink_backing(&upper_dir, name, context),
            Err(SystemError::ENOENT) if lower_positive => {
                inode.create_whiteout_locked(name, context)
            }
            Err(SystemError::ENOENT) => Err(SystemError::ENOENT),
            Err(err) => Err(err),
        }
    } else if lower_positive {
        inode.create_whiteout_locked(name, context)
    } else {
        Err(SystemError::ENOENT)
    };
    if result.is_ok() {
        state.modified(&[name]);
    }
    result
}

fn unlink_backing(
    inode: &Arc<dyn IndexNode>,
    name: &str,
    context: Option<&DentryMutationContext<'_>>,
) -> Result<(), SystemError> {
    match context {
        Some(context) => inode.unlink_with_context(name, context),
        None => inode.unlink(name),
    }
}

fn rmdir_backing(
    inode: &Arc<dyn IndexNode>,
    name: &str,
    context: Option<&DentryMutationContext<'_>>,
) -> Result<(), SystemError> {
    match context {
        Some(context) => inode.rmdir_with_context(name, context),
        None => inode.rmdir(name),
    }
}

pub(super) fn link(
    inode: &OvlInode,
    name: &str,
    other: &Arc<dyn IndexNode>,
) -> Result<(), system_error::SystemError> {
    let fs = inode.overlay_fs()?;
    let state = inode.dir_state()?;
    let _mutation_guard = state.mutation_lock.lock();

    match super::lookup::find_locked(inode, name, &state) {
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

    let result = create_over_whiteout(
        inode,
        name,
        |dir, temp_name| {
            dir.link(temp_name, &source_upper)?;
            Ok(source_upper.clone())
        },
        false,
    )
    .map(|_| ());
    if result.is_ok() {
        state.modified(&[name]);
    }
    result
}

pub(super) fn create(
    inode: &OvlInode,
    name: &str,
    file_type: vfs::FileType,
    mode: vfs::InodeMode,
) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
    let state = inode.dir_state()?;
    let _mutation_guard = state.mutation_lock.lock();
    let result = create_over_whiteout(
        inode,
        name,
        |dir, temp_name| dir.create(temp_name, file_type, mode),
        file_type == FileType::Dir,
    );
    if result.is_ok() {
        state.modified(&[name]);
    }
    result
}

pub(super) fn mknod(
    inode: &OvlInode,
    filename: &str,
    mode: vfs::InodeMode,
    dev_t: DeviceNumber,
) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
    let state = inode.dir_state()?;
    let _mutation_guard = state.mutation_lock.lock();
    if FileType::from(mode) == FileType::CharDevice && dev_t == WHITEOUT_DEV {
        return Err(SystemError::EPERM);
    }

    let result = create_over_whiteout(
        inode,
        filename,
        |dir, temp_name| dir.mknod(temp_name, mode, dev_t),
        FileType::from(mode) == FileType::Dir,
    );
    if result.is_ok() {
        state.modified(&[filename]);
    }
    result
}

pub(super) fn is_dir_empty_locked(inode: &OvlInode, state: &DirState) -> Result<bool, SystemError> {
    Ok(super::readdir::list_locked(inode, state)?
        .iter()
        .all(|entry| is_dot_entry(entry)))
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

    let (workdir, _temp_inode, temp_name) = inode.create_workdir_temp(create_temp)?;
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

    upper_inode.find(name)
}

fn is_dot_entry(name: &str) -> bool {
    name == "." || name == ".."
}
