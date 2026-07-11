use super::dir;
use super::inode::{DirState, OvlInode};
use crate::filesystem::vfs::{syscall::RenameFlags, IndexNode};
use crate::libs::casting::DowncastArc;
use alloc::sync::Arc;
use system_error::SystemError;

pub(super) fn move_to(
    inode: &OvlInode,
    old_name: &str,
    target: &Arc<dyn IndexNode>,
    new_name: &str,
    flags: RenameFlags,
) -> Result<(), SystemError> {
    if flags.contains(RenameFlags::WHITEOUT) {
        return Err(SystemError::EINVAL);
    }

    let target_ovl = target
        .clone()
        .downcast_arc::<OvlInode>()
        .ok_or(SystemError::EXDEV)?;

    if inode.redirect == target_ovl.redirect && old_name == new_name {
        return Ok(());
    }

    let fs = inode.overlay_fs()?;
    let source_state = inode.dir_state()?;
    let target_state = target_ovl.dir_state()?;
    let _commit_guard = fs.mutation_lock.lock();
    let result = if Arc::ptr_eq(&source_state, &target_state) {
        let _guard = source_state.mutation_lock.lock();
        move_to_locked(
            inode,
            old_name,
            &target_ovl,
            new_name,
            flags,
            &source_state,
            &target_state,
        )
    } else if Arc::as_ptr(&source_state) < Arc::as_ptr(&target_state) {
        let _source_guard = source_state.mutation_lock.lock();
        let _target_guard = target_state.mutation_lock.lock();
        move_to_locked(
            inode,
            old_name,
            &target_ovl,
            new_name,
            flags,
            &source_state,
            &target_state,
        )
    } else {
        let _target_guard = target_state.mutation_lock.lock();
        let _source_guard = source_state.mutation_lock.lock();
        move_to_locked(
            inode,
            old_name,
            &target_ovl,
            new_name,
            flags,
            &source_state,
            &target_state,
        )
    };
    if result.is_ok() {
        source_state.modified(&[old_name, new_name]);
        if !Arc::ptr_eq(&source_state, &target_state) {
            target_state.modified(&[old_name, new_name]);
        }
    }
    result
}

fn move_to_locked(
    inode: &OvlInode,
    old_name: &str,
    target_ovl: &Arc<OvlInode>,
    new_name: &str,
    flags: RenameFlags,
    source_state: &DirState,
    target_state: &DirState,
) -> Result<(), SystemError> {

    let source = inode.lookup_overlay_child_locked(old_name, source_state)?;
    let target_had_whiteout = target_ovl.has_whiteout(new_name);
    let target_child = match target_ovl.lookup_overlay_child_locked(new_name, target_state) {
        Ok(found) => Some(found),
        Err(SystemError::ENOENT) => None,
        Err(err) => return Err(err),
    };

    if flags.contains(RenameFlags::NOREPLACE) && target_child.is_some() {
        return Err(SystemError::EEXIST);
    }

    if flags.contains(RenameFlags::EXCHANGE) {
        let target_child = target_child.ok_or(SystemError::ENOENT)?;
        if (source.is_dir() && source.has_lower())
            || (target_child.is_dir() && target_child.has_lower())
        {
            return Err(SystemError::EXDEV);
        }

        source.copy_up_locked()?;
        target_child.copy_up_locked()?;
        let old_upper_dir = inode.writable_upper_inode_locked()?;
        let new_upper_dir = target_ovl.writable_upper_inode_locked()?;
        return old_upper_dir.move_to(old_name, &new_upper_dir, new_name, flags);
    }

    let source_needs_whiteout = inode.lower_positive(old_name);
    let source_has_lower_tree = source.is_dir() && source.has_lower();
    if source_has_lower_tree {
        return Err(SystemError::EXDEV);
    }

    if let Some(target_child) = target_child {
        if source.is_dir() && !target_child.is_dir() {
            return Err(SystemError::ENOTDIR);
        }
        if !source.is_dir() && target_child.is_dir() {
            return Err(SystemError::EISDIR);
        }
        if source.is_dir() && target_child.is_dir() {
            let target_node: Arc<dyn IndexNode> = target_child.clone();
            if !dir::is_dir_empty(&target_node)? {
                return Err(SystemError::ENOTEMPTY);
            }
        }
    }

    if !source.is_pure_upper() {
        source.copy_up_locked()?;
    }

    let old_upper_dir = inode.writable_upper_inode_locked()?;
    let new_upper_dir = target_ovl.writable_upper_inode_locked()?;
    let mut upper_flags = flags;
    if target_had_whiteout {
        upper_flags.remove(RenameFlags::NOREPLACE);
        if source_needs_whiteout {
            return old_upper_dir.move_to(
                old_name,
                &new_upper_dir,
                new_name,
                RenameFlags::EXCHANGE,
            );
        }
        if source.is_dir() {
            old_upper_dir.move_to(old_name, &new_upper_dir, new_name, RenameFlags::EXCHANGE)?;
            let _ = OvlInode::cleanup_workdir_temp(&old_upper_dir, old_name);
            return Ok(());
        }
    }
    if source_needs_whiteout {
        upper_flags.insert(RenameFlags::WHITEOUT);
    }
    old_upper_dir.move_to(old_name, &new_upper_dir, new_name, upper_flags)
}
