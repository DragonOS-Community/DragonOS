//! System call handler for sys_mount.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MOUNT},
    filesystem::vfs::{
        fcntl::AtFlags,
        mount::{
            is_mountpoint_root, with_topology_snapshot, MountFSInode, MountFlags,
            MOUNT_LIFECYCLE_LOCK,
        },
        produce_fs,
        utils::user_path_at,
        FileType, FsReconfigureRequest, IndexNode, MountFS, MAX_PATHLEN,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::{
        cred::{ns_capable, CAPFlags},
        namespace::propagation::{
            change_mnt_propagation_recursive, flags_to_propagation_type, is_propagation_change,
        },
        ProcessManager,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access,
    },
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

/// # Mount filesystem
///
/// Handles the Linux-compatible `mount(2)` syscall entry point.
///
/// Depending on `mountflags`, this may create a new mount, bind mount, remount,
/// move an existing mount, or change mount propagation attributes.
///
/// ## Parameters:
///
/// - `source`: source path/device string, or the mount path being moved for `MS_MOVE`
/// - `target`: target mount point path
/// - `filesystemtype`: filesystem type for new mounts; ignored by bind/move/propagation changes
/// - `mountflags`: Linux `MS_*` mount flags
/// - `data`: filesystem-specific mount data
///
/// ## Return value
/// - `Ok(0)`: mount operation completed successfully
/// - `Err(SystemError)`: mount operation failed
pub struct SysMountHandle;

impl Syscall for SysMountHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let source = Self::source(args);
        let target = Self::target(args);
        let filesystemtype = Self::filesystemtype(args);
        let data = Self::raw_data(args);
        let raw_mount_flags = Self::mountflags(args);
        // log::debug!(
        //     "sys_mount: source: {:?}, target: {:?}, filesystemtype: {:?}, mount_flags: {:?}, data: {:?}",
        //     source, target, filesystemtype, mount_flags, data
        // );
        let mount_flags = MountFlags::from_bits_truncate(raw_mount_flags);
        if is_propagation_change(mount_flags) && raw_mount_flags & !MountFlags::all().bits() != 0 {
            return Err(SystemError::EINVAL);
        }

        let target = copy_mount_path_string(target).inspect_err(|e| {
            log::error!("Failed to read mount target: {:?}", e);
        })?;
        let source = copy_mount_path_string(source).inspect_err(|e| {
            log::error!("Failed to read mount source: {:?}", e);
        })?;

        let data = copy_mount_string(data).inspect_err(|e| {
            log::error!("Failed to read mount data: {:?}", e);
        })?;

        let fstype_str = copy_mount_string(filesystemtype).inspect_err(|e| {
            log::error!("Failed to read filesystem type: {:?}", e);
        })?;

        do_mount(source, target, fstype_str, data, mount_flags)?;

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let flags = MountFlags::from_bits(Self::mountflags(args)).unwrap_or(MountFlags::empty());

        vec![
            FormattedSyscallParam::new("source", format!("{:?}", Self::source(args))),
            FormattedSyscallParam::new("target", format!("{:?}", Self::target(args))),
            FormattedSyscallParam::new(
                "filesystem type",
                format!("{:?}", Self::filesystemtype(args)),
            ),
            FormattedSyscallParam::new(
                "mountflags",
                format!("{:?} ({:#x})", flags, Self::mountflags(args)),
            ),
            FormattedSyscallParam::new("data", format!("{:?}", Self::raw_data(args))),
        ]
    }
}

impl SysMountHandle {
    fn source(args: &[usize]) -> Option<*const u8> {
        let source = args[0] as *const u8;
        if source.is_null() {
            None
        } else {
            Some(source)
        }
    }
    fn target(args: &[usize]) -> Option<*const u8> {
        let target = args[1] as *const u8;
        if target.is_null() {
            None
        } else {
            Some(target)
        }
    }
    fn filesystemtype(args: &[usize]) -> Option<*const u8> {
        let p = args[2] as *const u8;
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }
    fn mountflags(args: &[usize]) -> u32 {
        args[3] as u32
    }
    fn raw_data(args: &[usize]) -> Option<*const u8> {
        let raw = args[4] as *const u8;
        if raw.is_null() {
            return None;
        }

        Some(raw)
    }
}

syscall_table_macros::declare_syscall!(SYS_MOUNT, SysMountHandle);

/// Linux `may_mount()`: modifying the current mount namespace requires
/// CAP_SYS_ADMIN in that namespace's owning user namespace.
pub(super) fn may_mount() -> bool {
    let current_mntns = ProcessManager::current_mntns();
    ns_capable(current_mntns.user_ns(), CAPFlags::CAP_SYS_ADMIN)
}

/// # do_mount - Dispatch a mount operation
///
/// Resolves `target` in the current mount namespace and dispatches the request
/// according to `mount_flags`.
///
/// ## Arguments
///
/// - `source`: source path/device string, or the mount path being moved for `MS_MOVE`.
/// - `target`: target mount point path from userspace.
/// - `filesystemtype`: filesystem type for new mounts.
/// - `data`: filesystem-specific mount data.
/// - `mount_flags`: Linux `MS_*` mount flags.
///
/// ## Return value
///
/// - `Ok(())`: mount operation completed successfully.
/// - `Err(SystemError)`: Returns an error on failure.
pub fn do_mount(
    source: Option<String>,
    target: Option<String>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mount_flags: MountFlags,
) -> Result<(), SystemError> {
    let requested_target = target.as_deref().unwrap_or("");
    let (current_node, rest_path) = user_path_at(
        &ProcessManager::current_pcb(),
        AtFlags::AT_FDCWD.bits(),
        requested_target,
    )?;
    let inode = current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    path_mount(source, inode, filesystemtype, data, mount_flags)
}

fn path_mount(
    source: Option<String>,
    target_inode: Arc<dyn IndexNode>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mut flags: MountFlags,
) -> Result<(), SystemError> {
    if flags & MountFlags::MGC_MASK == MountFlags::MGC_VAL {
        flags.remove(MountFlags::MGC_MASK);
    }

    if flags.contains(MountFlags::NOUSER) {
        return Err(SystemError::EINVAL);
    }

    if !may_mount() {
        return Err(SystemError::EPERM);
    }

    let mut mnt_flags = MountFlags::empty();

    // Default to relatime unless overriden
    if !flags.contains(MountFlags::NOATIME) {
        mnt_flags.insert(MountFlags::RELATIME);
    }
    if flags.contains(MountFlags::NOSUID) {
        mnt_flags.insert(MountFlags::NOSUID);
    }
    if flags.contains(MountFlags::NODEV) {
        mnt_flags.insert(MountFlags::NODEV);
    }
    if flags.contains(MountFlags::NOEXEC) {
        mnt_flags.insert(MountFlags::NOEXEC);
    }
    if flags.contains(MountFlags::NOATIME) {
        mnt_flags.insert(MountFlags::NOATIME);
    }
    if flags.contains(MountFlags::NODIRATIME) {
        mnt_flags.insert(MountFlags::NODIRATIME);
    }
    if flags.contains(MountFlags::STRICTATIME) {
        mnt_flags.remove(MountFlags::RELATIME);
        mnt_flags.remove(MountFlags::NOATIME);
    }
    if flags.contains(MountFlags::RDONLY) {
        mnt_flags.insert(MountFlags::RDONLY);
    }
    if flags.contains(MountFlags::NOSYMFOLLOW) {
        mnt_flags.insert(MountFlags::NOSYMFOLLOW);
    }
    let sb_flags = flags
        & (MountFlags::RDONLY
            | MountFlags::SYNCHRONOUS
            | MountFlags::MANDLOCK
            | MountFlags::DIRSYNC
            | MountFlags::SILENT
            | MountFlags::POSIXACL
            | MountFlags::LAZYTIME
            | MountFlags::I_VERSION);

    // MS_REMOUNT|MS_BIND and MS_REMOUNT share this atime preservation logic.
    if flags.contains(MountFlags::REMOUNT)
        && !flags.intersects(
            MountFlags::NOATIME
                | MountFlags::NODIRATIME
                | MountFlags::RELATIME
                | MountFlags::STRICTATIME,
        )
    {
        let target_mfs = target_inode
            .fs()
            .downcast_arc::<MountFS>()
            .ok_or(SystemError::EINVAL)?;
        let current_atime = target_mfs.mount_flags() & MountFlags::MNT_ATIME_MASK;
        mnt_flags.remove(MountFlags::MNT_ATIME_MASK);
        mnt_flags.insert(current_atime);
    }

    if flags.intersection(MountFlags::REMOUNT | MountFlags::BIND)
        == (MountFlags::REMOUNT | MountFlags::BIND)
    {
        return do_reconfigure_bind_mount(target_inode, mnt_flags);
    }

    if flags.contains(MountFlags::REMOUNT) {
        return do_remount(target_inode, sb_flags, mnt_flags, data);
    }

    if flags.contains(MountFlags::BIND) {
        return do_bind_mount(source, target_inode, flags);
    }
    // Handle propagation type changes (mount --make-{shared,private,slave,unbindable})
    if is_propagation_change(flags) {
        return do_change_type(target_inode, flags);
    }

    if flags.contains(MountFlags::MOVE) {
        return do_move_mount(source, target_inode);
    }

    // Create a new mount
    return do_new_mount(source, target_inode, filesystemtype, data, mnt_flags).map(|_| ());
}

/// Modify the mount flags of an existing mount.
///
/// Linux has two independent paths:
/// - do_reconfigure_mnt(): MS_REMOUNT|MS_BIND, down_read(sb), only changes mount flags ← this function
/// - do_remount(): MS_REMOUNT alone, down_write(sb) + reconfigure_super() + set_mount_attributes() ← TODO
fn do_reconfigure_bind_mount(
    target_inode: Arc<dyn IndexNode>,
    requested_flags: MountFlags,
) -> Result<(), SystemError> {
    if !is_mountpoint_root(&target_inode) {
        return Err(SystemError::EINVAL);
    }

    let target_mfs = target_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;

    // The target mount must belong to the current process's mount namespace.
    let current_mntns = ProcessManager::current_mntns();
    if !target_mfs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }

    // Preserve unmodifiable flags, only overwrite SETTABLE bits.
    let _topology = MOUNT_LIFECYCLE_LOCK.lock();
    if !target_mfs.is_live() || !target_mfs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }
    target_mfs.update_mount_flags(|mount_flags| {
        let preserved = *mount_flags & !MountFlags::MNT_USER_SETTABLE_MASK;
        let new_settable = requested_flags & MountFlags::MNT_USER_SETTABLE_MASK;
        *mount_flags = preserved | new_settable;
    });

    Ok(())
}

fn do_remount(
    target_inode: Arc<dyn IndexNode>,
    requested_sb_flags: MountFlags,
    requested_mnt_flags: MountFlags,
    data: Option<String>,
) -> Result<(), SystemError> {
    if !is_mountpoint_root(&target_inode) {
        return Err(SystemError::EINVAL);
    }

    let target_mfs = target_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;

    let current_mntns = ProcessManager::current_mntns();
    if !target_mfs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }

    // Serialize the filesystem reconfigure operation per superblock. The
    // potentially sleeping callback stays outside the global topology lock;
    // only the final paired SB/mount flag publication is topology-atomic.
    let super_block_state = target_mfs.super_block_state();
    let _reconfigure = super_block_state.umount_write();
    {
        // Linearize admission after excluding final shutdown. If detach won
        // first, fail before invoking a stateful filesystem callback; if we
        // win, the SB write guard keeps the backend alive through commit.
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        if !target_mfs.is_live() || !target_mfs.is_belongs_to_mntns(&current_mntns) {
            return Err(SystemError::EINVAL);
        }
    }
    let old_sb_flags = target_mfs.super_block_flags();
    let (data_sb_flags, data_sb_flags_mask, fs_private_data) = parse_remount_data(data.as_deref())?;
    let sb_flags_mask = MountFlags::RMT_MASK | data_sb_flags_mask;
    let requested_sb_flags = (requested_sb_flags & !data_sb_flags_mask) | data_sb_flags;
    let new_sb_flags = (old_sb_flags & !sb_flags_mask) | (requested_sb_flags & sb_flags_mask);

    if new_sb_flags.contains(MountFlags::RDONLY)
        && !old_sb_flags.contains(MountFlags::RDONLY)
        && target_mfs.has_writers()
    {
        return Err(SystemError::EBUSY);
    }

    let effective_sb_flags = target_mfs
        .inner_filesystem()
        .reconfigure(FsReconfigureRequest {
            sb_flags: new_sb_flags,
            sb_flags_mask,
            raw_data: fs_private_data.as_deref(),
            oldapi: true,
        })?;

    let _topology = MOUNT_LIFECYCLE_LOCK.lock();
    // reconfigure() may already have committed filesystem-private state. A
    // concurrent lazy detach must not turn that successful operation into a
    // partial-commit error; the SB write guard keeps final shutdown out until
    // the paired flag publication completes.
    target_mfs.set_super_block_flags(effective_sb_flags);
    target_mfs.update_mount_flags(|mount_flags| {
        let preserved = *mount_flags & !MountFlags::MNT_USER_SETTABLE_MASK;
        let new_settable = requested_mnt_flags & MountFlags::MNT_USER_SETTABLE_MASK;
        *mount_flags = preserved | new_settable;
    });

    Ok(())
}

fn parse_remount_data(
    data: Option<&str>,
) -> Result<(MountFlags, MountFlags, Option<String>), SystemError> {
    let Some(data) = data else {
        return Ok((MountFlags::empty(), MountFlags::empty(), None));
    };

    let mut sb_flags = MountFlags::empty();
    let mut sb_flags_mask = MountFlags::empty();
    let mut private_data = String::new();

    for opt in data.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match opt {
            "ro" => {
                sb_flags.insert(MountFlags::RDONLY);
                sb_flags_mask.insert(MountFlags::RDONLY);
            }
            "rw" => {
                sb_flags.remove(MountFlags::RDONLY);
                sb_flags_mask.insert(MountFlags::RDONLY);
            }
            "sync" => {
                sb_flags.insert(MountFlags::SYNCHRONOUS);
                sb_flags_mask.insert(MountFlags::SYNCHRONOUS);
            }
            "async" => {
                sb_flags.remove(MountFlags::SYNCHRONOUS);
                sb_flags_mask.insert(MountFlags::SYNCHRONOUS);
            }
            "lazytime" => {
                sb_flags.insert(MountFlags::LAZYTIME);
                sb_flags_mask.insert(MountFlags::LAZYTIME);
            }
            "nolazytime" => {
                sb_flags.remove(MountFlags::LAZYTIME);
                sb_flags_mask.insert(MountFlags::LAZYTIME);
            }
            "mand" => {
                sb_flags.insert(MountFlags::MANDLOCK);
                sb_flags_mask.insert(MountFlags::MANDLOCK);
            }
            "nomand" => {
                sb_flags.remove(MountFlags::MANDLOCK);
                sb_flags_mask.insert(MountFlags::MANDLOCK);
            }
            _ => {
                if !private_data.is_empty() {
                    private_data.push(',');
                }
                private_data.push_str(opt);
            }
        }
    }

    let private_data = if private_data.is_empty() {
        None
    } else {
        Some(private_data)
    };
    Ok((sb_flags, sb_flags_mask, private_data))
}

fn do_new_mount(
    source: Option<String>,
    target_inode: Arc<dyn IndexNode>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mount_flags: MountFlags,
) -> Result<Arc<MountFS>, SystemError> {
    let fs_type_str = filesystemtype.ok_or(SystemError::EINVAL)?;
    let source = source.ok_or(SystemError::EINVAL)?;
    let fs = produce_fs(&fs_type_str, data.as_deref(), &source, mount_flags).inspect_err(|e| {
        log::warn!("Failed to produce filesystem: {:?}", e);
    })?;

    let new_mount_res: Result<Arc<MountFS>, SystemError> =
        if let Some(mnt_inode) = target_inode.clone().downcast_arc::<MountFSInode>() {
            let (to_mount_fs, root_inner_inode) = fs
                .clone()
                .downcast_arc::<MountFS>()
                .map(|it| (it.inner_filesystem(), it.root_inner_inode()))
                .unwrap_or_else(|| {
                    let root_inner_inode = fs.root_inode();
                    (fs, root_inner_inode)
                });

            let prepared = mnt_inode.prepare_subtree_with_root_dentry(
                to_mount_fs,
                root_inner_inode,
                None,
                mount_flags,
                None,
                None,
            )?;
            prepared.set_mount_source(Some(source.clone()));
            if let Err(error) = mnt_inode.publish_prepared_subtree(&prepared) {
                MountFS::deactivate_disconnected_subtree(&prepared);
                return Err(error);
            }
            Ok(prepared)
        } else {
            target_inode.mount(fs, mount_flags)
        };

    let new_mount = new_mount_res?;
    // Legacy non-MountFS targets cannot propagate before this point. Normal
    // namespace mounts set the source on their detached MountFS above.
    if new_mount.mount_source().is_none() {
        new_mount.set_mount_source(Some(source));
    }

    Ok(new_mount)
}
#[inline(never)]
fn copy_mount_string(raw: Option<*const u8>) -> Result<Option<String>, SystemError> {
    if let Some(raw) = raw {
        let s = user_access::check_and_clone_cstr(raw, Some(MAX_PATHLEN))
            .inspect_err(|e| {
                log::error!("Failed to read mount string: {:?}", e);
            })?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Ok(Some(s))
    } else {
        Ok(None)
    }
}

#[inline(never)]
fn copy_mount_path_string(raw: Option<*const u8>) -> Result<Option<String>, SystemError> {
    if let Some(raw) = raw {
        let s = user_access::vfs_check_and_clone_cstr(raw, Some(MAX_PATHLEN))
            .inspect_err(|e| {
                log::error!("Failed to read mount path string: {:?}", e);
            })?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Ok(Some(s))
    } else {
        Ok(None)
    }
}

/// Perform a bind mount operation.
///
/// Bind mount makes a directory subtree visible at another location.
/// The source and target will share the same underlying filesystem content.
///
/// # Arguments
/// * `source` - The source path to bind from
/// * `target_inode` - The target mount point inode
/// * `flags` - Mount flags (MS_BIND, optionally MS_REC)
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
fn do_bind_mount(
    source: Option<String>,
    target_inode: Arc<dyn IndexNode>,
    flags: MountFlags,
) -> Result<(), SystemError> {
    let source_path = source.ok_or(SystemError::EINVAL)?;

    // log::debug!(
    //     "do_bind_mount: source={}, recursive={}",
    //     source_path,
    //     flags.contains(MountFlags::REC)
    // );

    // Resolve the source path to get the source inode
    let (current_node, rest_path) = user_path_at(
        &ProcessManager::current_pcb(),
        AtFlags::AT_FDCWD.bits(),
        &source_path,
    )?;
    let source_inode =
        current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    let source_is_dir = source_inode.metadata()?.file_type == FileType::Dir;
    let target_is_dir = target_inode.metadata()?.file_type == FileType::Dir;
    if source_is_dir != target_is_dir {
        return Err(SystemError::ENOTDIR);
    }

    let source_mount_inode = source_inode
        .clone()
        .downcast_arc::<MountFSInode>()
        .ok_or(SystemError::EINVAL)?;

    let source_mfs = source_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;

    let target_mountpoint = target_inode
        .clone()
        .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
        .ok_or(SystemError::EINVAL)?;
    let target_mount_fs = target_mountpoint.mount_fs();
    let current_mntns = ProcessManager::current_mntns();

    // Linux holds namespace_lock across check_mnt(), clone_mnt(root), and
    // copy_tree(children). Take one mount+dentry snapshot for the equivalent
    // source validation and complete detached clone so root and descendants
    // cannot observe different lifecycle or propagation states.
    let target_mfs = with_topology_snapshot(|| {
        if !source_mfs.is_live()
            || !source_mfs.is_belongs_to_mntns(&current_mntns)
            || !target_mount_fs.is_live()
            || !target_mount_fs.is_belongs_to_mntns(&current_mntns)
        {
            return Err(SystemError::EINVAL);
        }
        if source_mfs.propagation().is_unbindable() {
            return Err(SystemError::EINVAL);
        }
        if !flags.contains(MountFlags::REC)
            && has_locked_children_in_view_locked(&source_mfs, &source_mount_inode)?
        {
            return Err(SystemError::EINVAL);
        }

        let root_inner_inode = if is_mountpoint_root(&source_inode) {
            source_mfs.root_inner_inode()
        } else {
            source_mount_inode.underlying_inode()
        };
        let target_mfs = target_mountpoint.prepare_subtree_with_root_dentry_prevalidated(
            source_mfs.inner_filesystem(),
            root_inner_inode,
            Some(source_mount_inode.shared_dentry()),
            source_mfs.mount_flags(),
            Some(source_mfs.super_block_state()),
            Some(&source_mfs),
        )?;
        target_mfs.set_mount_source(Some(source_path.clone()));

        // The root edge is the publication point, so lookup never observes a
        // half-copied recursive bind tree.
        if flags.contains(MountFlags::REC) {
            if let Err(error) = do_recursive_bind_mount_locked(&source_mfs, &target_mfs) {
                MountFS::deactivate_disconnected_subtree(&target_mfs);
                return Err(error);
            }
        }
        Ok(target_mfs)
    })?;

    if let Err(error) = target_mountpoint.publish_prepared_subtree(&target_mfs) {
        MountFS::deactivate_disconnected_subtree(&target_mfs);
        return Err(error);
    }

    Ok(())
}

/// Linux rejects a non-recursive bind when it would uncover locked child
/// mounts below the selected source dentry (has_locked_children()).
/// Caller holds the mount+dentry topology snapshot.
fn has_locked_children_in_view_locked(
    source_mount: &Arc<MountFS>,
    source_root: &Arc<MountFSInode>,
) -> Result<bool, SystemError> {
    let mut pending = source_mount.mount_children();
    while let Some(child) = pending.pop() {
        let mountpoint = child.self_mountpoint().ok_or(SystemError::EINVAL)?;
        if mountpoint
            .relative_path_from_snapshot(source_root)?
            .is_none()
        {
            continue;
        }
        if child.is_locked() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Change the propagation type of a mount point.
///
/// This implements the kernel path for `mount --make-{shared,private,slave,unbindable}`.
///
/// # Arguments
/// * `target_inode` - The mount point to change
/// * `flags` - Mount flags containing the propagation type
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
///
/// Namespace isolation is implicitly guaranteed by VFS path lookup.
fn do_change_type(target_inode: Arc<dyn IndexNode>, flags: MountFlags) -> Result<(), SystemError> {
    // Target must be a mount root
    if !is_mountpoint_root(&target_inode) {
        return Err(SystemError::EINVAL);
    }

    let prop_type = flags_to_propagation_type(flags)?;

    // Check if recursive flag is set
    let recursive = flags.contains(MountFlags::REC);

    // Get the MountFS from the inode
    let mount_fs = target_inode.fs().downcast_arc::<MountFS>().ok_or_else(|| {
        log::warn!("do_change_type: target is not a mounted filesystem");
        SystemError::EINVAL
    })?;

    // log::debug!(
    //     "do_change_type: changing propagation to {:?}, recursive={}",
    //     prop_type,
    //     recursive
    // );

    // Change the propagation type
    change_mnt_propagation_recursive(&mount_fs, prop_type, recursive)?;

    Ok(())
}

/// Implement mount(MS_MOVE): move an already-mounted mount (along with its entire subtree) to a new mount point.
///
/// Aligns with Linux `do_move_mount` (fs/namespace.c). The calling convention is
/// `mount(source, target, NULL, MS_MOVE, NULL)`, where `source` is the path of the
/// mount being moved (not a device name). `MS_REC` is meaningless for move, as move
/// inherently moves the entire subtree.
///
/// # Arguments
/// * `source` - The source path of the mount being moved.
/// * `target_inode` - The target inode of the new mount point.
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
fn do_move_mount(
    source: Option<String>,
    target_inode: Arc<dyn IndexNode>,
) -> Result<(), SystemError> {
    let source_path = source.ok_or(SystemError::EINVAL)?;
    if source_path.is_empty() {
        return Err(SystemError::EINVAL);
    }

    // Resolve source path → inode.
    let (begin, rest) = user_path_at(
        &ProcessManager::current_pcb(),
        AtFlags::AT_FDCWD.bits(),
        &source_path,
    )?;
    let source_inode = begin.lookup_follow_symlink(&rest, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    let current_mntns = ProcessManager::current_mntns();

    // path_mounted(old_path): source must be a mount root, not an ordinary subdirectory within a mount.
    if !is_mountpoint_root(&source_inode) {
        return Err(SystemError::EINVAL);
    }

    // Resolve the mount object. Namespace membership, lifecycle, locking,
    // propagation constraints and cycle prevention are validated atomically by
    // MntNamespace::move_mount() under the topology lock.
    let source_mfs = source_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;

    // d_is_dir(new) != d_is_dir(old): source and target types must match.
    let source_is_dir = source_inode.metadata()?.file_type == FileType::Dir;
    let target_is_dir = target_inode.metadata()?.file_type == FileType::Dir;
    if source_is_dir != target_is_dir {
        return Err(SystemError::ENOTDIR);
    }

    // Target mount point inode (for topology attachment and propagation).
    let target_mountpoint = target_inode
        .clone()
        .downcast_arc::<MountFSInode>()
        .ok_or(SystemError::EINVAL)?;
    current_mntns.move_mount(&source_mfs, &target_mountpoint)?;

    Ok(())
}

/// Recursively bind mount all submounts from source to target.
///
/// This function traverses the source mount tree and creates corresponding
/// bind mounts at the target location for all submounts.
///
/// # Arguments
/// * `source_mfs` - The source MountFS to copy submounts from
/// * `target_mfs` - The target bind clone corresponding to `source_mfs`.
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
///
/// Caller holds the mount+dentry topology snapshot for the complete detached copy.
fn do_recursive_bind_mount_locked(
    source_mfs: &Arc<MountFS>,
    target_mfs: &Arc<MountFS>,
) -> Result<(), SystemError> {
    let mut pending = vec![(source_mfs.clone(), target_mfs.clone())];
    while let Some((source_parent, target_parent)) = pending.pop() {
        for source_child in source_parent.mount_children() {
            let source_mountpoint = source_child.self_mountpoint().ok_or(SystemError::EINVAL)?;
            let target_mountpoint =
                match target_parent.wrapper_for_dentry(source_mountpoint.shared_dentry()) {
                    Ok(mountpoint) => mountpoint,
                    Err(SystemError::EXDEV) => continue,
                    Err(error) => return Err(error),
                };

            // Match Linux copy_tree(): first discard mounts outside the
            // selected source view, then apply unbindable/locked semantics.
            // The recursive-bind root was rejected by do_bind_mount above.
            if source_child.propagation().is_unbindable() {
                if source_child.is_locked() {
                    return Err(SystemError::EPERM);
                }
                continue;
            }

            let target_child = source_child.deepcopy(Some(target_mountpoint.clone()))?;
            if let Err(error) = target_parent.attach_top(&target_mountpoint, target_child.clone()) {
                MountFS::deactivate_disconnected_subtree(&target_child);
                return Err(error);
            }
            pending.push((source_child, target_child));
        }
    }

    Ok(())
}
