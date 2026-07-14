use super::dir;
use super::inode::{DirState, OvlInode};
use crate::filesystem::vfs::{mount::DentryMutationContext, syscall::RenameFlags, IndexNode};
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
    move_to_impl(inode, old_name, target, new_name, flags, None)
}

pub(super) fn move_to_with_context(
    inode: &OvlInode,
    old_name: &str,
    target: &Arc<dyn IndexNode>,
    new_name: &str,
    flags: RenameFlags,
    context: &DentryMutationContext<'_>,
) -> Result<(), SystemError> {
    move_to_impl(inode, old_name, target, new_name, flags, Some(context))
}

fn move_to_impl(
    inode: &OvlInode,
    old_name: &str,
    target: &Arc<dyn IndexNode>,
    new_name: &str,
    flags: RenameFlags,
    context: Option<&DentryMutationContext<'_>>,
) -> Result<(), SystemError> {
    if flags.contains(RenameFlags::WHITEOUT) {
        return Err(SystemError::EINVAL);
    }

    let target_ovl = target
        .clone()
        .downcast_arc::<OvlInode>()
        .ok_or(SystemError::EXDEV)?;

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
            (&source_state, &target_state),
            context,
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
            (&source_state, &target_state),
            context,
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
            (&source_state, &target_state),
            context,
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
    dir_states: (&DirState, &DirState),
    context: Option<&DentryMutationContext<'_>>,
) -> Result<(), SystemError> {
    let (source_state, target_state) = dir_states;
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

    if inode.redirect == target_ovl.redirect && old_name == new_name {
        return Ok(());
    }

    if flags.contains(RenameFlags::EXCHANGE) {
        let target_child = target_child.ok_or(SystemError::ENOENT)?;
        if (source.is_dir() && source.has_lower())
            || (target_child.is_dir() && target_child.has_lower())
        {
            return Err(SystemError::EXDEV);
        }

        copy_up_backing(&source, context)?;
        copy_up_backing(&target_child, context)?;
        let old_upper_dir = writable_upper_backing(inode, context)?;
        let new_upper_dir = writable_upper_backing(target_ovl, context)?;
        return move_backing(
            &old_upper_dir,
            old_name,
            &new_upper_dir,
            new_name,
            flags,
            context,
        );
    }

    let source_needs_whiteout = inode.lower_positive(old_name);
    let source_has_lower_tree = source.is_dir() && source.has_lower();
    if source_has_lower_tree {
        return Err(SystemError::EXDEV);
    }

    let target_child_state = if let Some(target_child) = target_child.as_ref() {
        if source.is_dir() && !target_child.is_dir() {
            return Err(SystemError::ENOTDIR);
        }
        if !source.is_dir() && target_child.is_dir() {
            return Err(SystemError::EISDIR);
        }
        if source.is_dir() && target_child.is_dir() {
            Some(target_child.dir_state()?)
        } else {
            None
        }
    } else {
        None
    };
    let _target_child_guard = target_child_state
        .as_ref()
        .map(|state| state.mutation_lock.lock());
    if let (Some(target_child), Some(target_child_state)) =
        (target_child.as_ref(), target_child_state.as_ref())
    {
        if !dir::is_dir_empty_locked(target_child, target_child_state)? {
            return Err(SystemError::ENOTEMPTY);
        }
    }

    if !source.is_pure_upper() {
        copy_up_backing(&source, context)?;
    }

    let old_upper_dir = writable_upper_backing(inode, context)?;
    let new_upper_dir = writable_upper_backing(target_ovl, context)?;
    let mut upper_flags = flags;
    if target_had_whiteout {
        upper_flags.remove(RenameFlags::NOREPLACE);
        if source_needs_whiteout {
            return move_backing(
                &old_upper_dir,
                old_name,
                &new_upper_dir,
                new_name,
                RenameFlags::EXCHANGE,
                context,
            );
        }
        if source.is_dir() {
            move_backing(
                &old_upper_dir,
                old_name,
                &new_upper_dir,
                new_name,
                RenameFlags::EXCHANGE,
                context,
            )?;
            let _ = OvlInode::cleanup_workdir_temp_with_context(&old_upper_dir, old_name, context);
            return Ok(());
        }
    }
    if source_needs_whiteout {
        upper_flags.insert(RenameFlags::WHITEOUT);
    }
    move_backing(
        &old_upper_dir,
        old_name,
        &new_upper_dir,
        new_name,
        upper_flags,
        context,
    )
}

fn move_backing(
    source: &Arc<dyn IndexNode>,
    old_name: &str,
    target: &Arc<dyn IndexNode>,
    new_name: &str,
    flags: RenameFlags,
    context: Option<&DentryMutationContext<'_>>,
) -> Result<(), SystemError> {
    match context {
        Some(context) => source.move_to_with_context(old_name, target, new_name, flags, context),
        None => source.move_to(old_name, target, new_name, flags),
    }
}

fn copy_up_backing(
    inode: &OvlInode,
    context: Option<&DentryMutationContext<'_>>,
) -> Result<(), SystemError> {
    match context {
        Some(context) => inode.copy_up_locked_with_context(context),
        None => inode.copy_up_locked(),
    }
}

fn writable_upper_backing(
    inode: &OvlInode,
    context: Option<&DentryMutationContext<'_>>,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    match context {
        Some(context) => inode.writable_upper_inode_locked_with_context(context),
        None => inode.writable_upper_inode_locked(),
    }
}
