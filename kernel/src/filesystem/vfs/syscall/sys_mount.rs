//! System call handler for sys_mount.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MOUNT},
    filesystem::vfs::{
        fcntl::AtFlags,
        mount::{is_mountpoint_root, MountFSInode, MountFlags, MountPath},
        produce_fs,
        utils::user_path_at,
        FileType, FsReconfigureRequest, IndexNode, InodeId, MountFS, MAX_PATHLEN,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::{
        namespace::propagation::{
            change_mnt_propagation_recursive, flags_to_propagation_type, is_propagation_change,
            propagate_moved_tree,
        },
        ProcessManager,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access,
    },
};
use alloc::string::String;
use alloc::string::ToString;
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
        let mount_flags = Self::mountflags(args);
        // log::debug!(
        //     "sys_mount: source: {:?}, target: {:?}, filesystemtype: {:?}, mount_flags: {:?}, data: {:?}",
        //     source, target, filesystemtype, mount_flags, data
        // );
        let mount_flags = MountFlags::from_bits_truncate(mount_flags);

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

/// # do_mount - Dispatch a mount operation
///
/// Resolves `target` in the current mount namespace and dispatches the request
/// according to `mount_flags`.
///
/// The resolved target path is passed down so `MountList` records the namespace
/// mount point rather than a filesystem-specific synthetic path such as
/// `fuse:<nodeid>`.
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
    let resolved_target_path = resolved_mount_target_path(requested_target, &inode)?;
    return path_mount(
        source,
        &resolved_target_path,
        inode,
        filesystemtype,
        data,
        mount_flags,
    );
}

fn resolved_mount_target_path(
    requested_path: &str,
    inode: &Arc<dyn IndexNode>,
) -> Result<String, SystemError> {
    match inode.absolute_path() {
        Ok(path) => Ok(path),
        Err(SystemError::ENOSYS) => Ok(normalize_requested_mount_path(requested_path)),
        Err(err) => Err(err),
    }
}

fn normalize_requested_mount_path(path: &str) -> String {
    let base = if path.starts_with('/') {
        String::from("/")
    } else {
        ProcessManager::current_pcb().basic().cwd()
    };

    let mut components: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for component in path.split('/').filter(|part| !part.is_empty()) {
        match component {
            "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(component),
        }
    }

    if components.is_empty() {
        return String::from("/");
    }

    let mut normalized = String::new();
    for component in components {
        normalized.push('/');
        normalized.push_str(component);
    }
    normalized
}

fn path_mount(
    source: Option<String>,
    target_path: &str,
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
        return do_bind_mount(source, target_path, target_inode, flags);
    }
    // Handle propagation type changes (mount --make-{shared,private,slave,unbindable})
    if is_propagation_change(flags) {
        return do_change_type(target_inode, flags);
    }

    if flags.contains(MountFlags::MOVE) {
        return do_move_mount(source, target_inode);
    }

    // Create a new mount
    return do_new_mount(
        source,
        target_path,
        target_inode,
        filesystemtype,
        data,
        mnt_flags,
    )
    .map(|_| ());
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
    target_path: &str,
    target_inode: Arc<dyn IndexNode>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mount_flags: MountFlags,
) -> Result<Arc<MountFS>, SystemError> {
    let fs_type_str = filesystemtype.ok_or(SystemError::EINVAL)?;
    let source = source.ok_or(SystemError::EINVAL)?;
    let fs = produce_fs(&fs_type_str, data.as_deref(), &source).inspect_err(|e| {
        log::warn!("Failed to produce filesystem: {:?}", e);
    })?;

    // target_path 已由 do_mount() 解析为命名空间绝对路径；不要在这里反查
    // target inner inode 的 absolute_path()，FUSE/virtiofs 可能返回合成路径。
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

            mnt_inode.mount_subtree_with_state(
                to_mount_fs,
                root_inner_inode,
                mount_flags,
                None,
                None,
                Some(Arc::new(MountPath::from(target_path))),
            )
        } else {
            target_inode.mount(fs, mount_flags)
        };

    let new_mount = new_mount_res?;
    new_mount.set_mount_source(Some(source));

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
/// * `target_path` - Resolved namespace path of the target mount point
/// * `target_inode` - The target mount point inode
/// * `flags` - Mount flags (MS_BIND, optionally MS_REC)
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
fn do_bind_mount(
    source: Option<String>,
    target_path: &str,
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

    // 穿越挂载点（含 FUSE announce-submounts 自动子挂载），避免 bind 仍绑定父 virtiofs 树上的 inode。
    let source_inode: Arc<dyn IndexNode> = source_inode
        .clone()
        .downcast_arc::<MountFSInode>()
        .map(|mnt| mnt.overlaid_inode() as Arc<dyn IndexNode>)
        .unwrap_or(source_inode);

    let source_is_dir = source_inode.metadata()?.file_type == FileType::Dir;
    let target_is_dir = target_inode.metadata()?.file_type == FileType::Dir;
    if source_is_dir != target_is_dir {
        return Err(SystemError::ENOTDIR);
    }

    // Get the source's filesystem
    let source_fs = source_inode.fs();

    // Check if source is on a MountFS
    let source_mfs = source_fs.clone().downcast_arc::<MountFS>();

    // The source mount must belong to the current mount namespace.
    if let Some(ref mfs) = source_mfs {
        let current_mntns = ProcessManager::current_mntns();
        if !mfs.is_belongs_to_mntns(&current_mntns) {
            return Err(SystemError::EINVAL);
        }
    }

    // Check if source is unbindable - if so, reject the bind mount
    if let Some(ref mfs) = source_mfs {
        if mfs.propagation().is_unbindable() {
            return Err(SystemError::EINVAL);
        }
    }

    // Clone source_mfs for recursive bind mount (need to keep it for later use)
    let source_mfs_for_recursive = source_mfs.clone();

    let root_inner_inode = if is_mountpoint_root(&source_inode) {
        source_mfs
            .as_ref()
            .map(|mfs| mfs.root_inner_inode())
            .unwrap_or_else(|| source_inode.clone())
    } else {
        source_inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|inode| inode.underlying_inode())
            .unwrap_or_else(|| source_inode.clone())
    };

    // Get the inner filesystem for mounting while preserving the source subtree root.
    let inner_fs = source_mfs
        .map(|mfs| mfs.inner_filesystem())
        .unwrap_or(source_fs);

    // do_loopback: the target mount point must belong to the current mount namespace.
    let current_mntns = ProcessManager::current_mntns();
    let target_mount_fs = target_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;
    if !target_mount_fs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }

    let target_mfs = target_inode
        .clone()
        .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
        .ok_or(SystemError::EINVAL)?
        .mount_subtree_with_state(
            inner_fs,
            root_inner_inode,
            MountFlags::empty(),
            source_mfs_for_recursive
                .as_ref()
                .map(|mfs| mfs.super_block_state()),
            source_mfs_for_recursive.as_ref(),
            Some(Arc::new(MountPath::from(target_path))),
        )?;
    target_mfs.set_mount_source(Some(source_path.clone()));

    // If MS_REC is set, recursively bind all submounts from source to target
    if flags.contains(MountFlags::REC) {
        if let Some(ref mfs) = source_mfs_for_recursive {
            // Linux kern_path() resolves the user path into a struct path; subsequent copy_tree
            // traverses submounts based on kernel mount/dentry data structures, without string path matching.
            // DragonOS uses strip_prefix for path matching, so source_path must be normalized to
            // the same format as mount_list storage (normalized absolute paths from absolute_path).
            // Passing the user's raw string directly would cause strip_prefix to fail on relative paths,
            // paths containing "..", or symlink paths, silently skipping all submounts.
            let resolved_source_path = match source_inode.absolute_path() {
                Ok(p) => p,
                Err(_) => {
                    // absolute_path failed (e.g., devfs device node).
                    // File-type bind mounts have no submounts; skipping recursion is safe.
                    return Ok(());
                }
            };
            let target_path = match target_inode.absolute_path() {
                Ok(p) => p,
                Err(_) => {
                    return Ok(());
                }
            };
            if let Err(e) =
                do_recursive_bind_mount(mfs, &target_mfs, &resolved_source_path, &target_path)
            {
                // When copy_tree fails, Linux calls umount_tree(res, UMOUNT_SYNC) to recursively roll back the entire subtree.
                // When graft_tree fails in do_loopback, Linux also calls umount_tree to roll back.
                // Ensures all-or-nothing atomic semantics.
                MountFS::umount_tree(&target_mfs);
                return Err(e);
            }
        }
    }

    Ok(())
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

    let prop_type = match flags_to_propagation_type(flags) {
        Some(t) => t,
        None => {
            log::warn!("do_change_type: no propagation flag set");
            return Err(SystemError::EINVAL);
        }
    };

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

    // check_mnt(p): the target mount point must belong to the current mount namespace.
    let target_parent_mfs = target_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;
    if !target_parent_mfs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }

    // path_mounted(old_path): source must be a mount root, not an ordinary subdirectory within a mount.
    if !is_mountpoint_root(&source_inode) {
        return Err(SystemError::EINVAL);
    }

    // is_mounted(old) + check_mnt(old): the mount being moved must belong to the current mount namespace.
    let source_mfs = source_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;
    if !source_mfs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }

    // Cannot move the namespace root (root mount's self_mountpoint is None).
    let source_mountpoint = source_mfs.self_mountpoint().ok_or(SystemError::EINVAL)?;
    let source_parent_mfs = source_mountpoint.mount_fs();

    // d_is_dir(new) != d_is_dir(old): source and target types must match.
    let source_is_dir = source_inode.metadata()?.file_type == FileType::Dir;
    let target_is_dir = target_inode.metadata()?.file_type == FileType::Dir;
    if source_is_dir != target_is_dir {
        return Err(SystemError::ENOTDIR);
    }

    // attached && IS_MNT_SHARED(parent): cannot move from a shared parent mount.
    if source_parent_mfs.propagation().is_shared() {
        return Err(SystemError::EINVAL);
    }

    // IS_MNT_SHARED(p) && tree_contains_unbindable(old):
    // A subtree containing unbindable mounts cannot be moved into a shared target.
    let target_shared = target_parent_mfs.propagation().is_shared();
    if target_shared && tree_contains_unbindable(&source_mfs) {
        return Err(SystemError::EINVAL);
    }

    // Cycle prevention: the target parent must not be the source itself or a descendant,
    // otherwise the subtree would be moved into itself.
    // Aligns with Linux `for (; mnt_has_parent(p); p = p->mnt_parent) if (p == old) goto out;`.
    let mut walk = target_parent_mfs.clone();
    loop {
        if Arc::ptr_eq(&walk, &source_mfs) {
            return Err(SystemError::EINVAL);
        }
        match walk.self_mountpoint() {
            Some(mp) => walk = mp.mount_fs(),
            None => break,
        }
    }

    // Target mount point inode (for topology attachment and propagation).
    let target_mountpoint = target_inode
        .clone()
        .downcast_arc::<MountFSInode>()
        .ok_or(SystemError::EINVAL)?;
    let target_mp_id = target_mountpoint.inode_id()?;

    // Perform topology move + mount_list subtree path rewrite.
    //
    // Use mount_list paths: virtiofs/FUSE absolute_path() may return "fuse:<nodeid>"
    // instead of the namespace mountpoint (e.g. /run/kata-containers/.../rootfs).
    let old_source_path = current_mntns
        .mount_list()
        .get_mount_path_by_mountfs(&source_mfs)
        .map(|p| p.as_str().to_string())
        .or_else(|| source_mountpoint.absolute_path().ok())
        .filter(|p| p.starts_with('/'))
        .ok_or(SystemError::EINVAL)?;
    let new_target_path = target_mountpoint
        .absolute_path()
        .ok()
        .filter(|p| p.starts_with('/'))
        .ok_or(SystemError::EINVAL)?;
    current_mntns.move_mount(
        &source_mfs,
        &target_mountpoint,
        &old_source_path,
        &new_target_path,
    )?;

    // Moved into a shared target: mark the entire subtree as shared and propagate to the target parent's peers.
    if target_shared {
        let new_path = Arc::new(MountPath::from(new_target_path));
        propagate_moved_tree(&target_parent_mfs, &source_mfs, target_mp_id, &new_path)?;
    }

    Ok(())
}

/// DFS check whether a mount subtree (including root) contains unbindable mounts.
fn tree_contains_unbindable(root: &Arc<MountFS>) -> bool {
    if root.propagation().is_unbindable() {
        return true;
    }
    let mut stack: Vec<Arc<MountFS>> = root.mountpoints().values().cloned().collect();
    while let Some(mnt) = stack.pop() {
        if mnt.propagation().is_unbindable() {
            return true;
        }
        for child in mnt.mountpoints().values() {
            stack.push(child.clone());
        }
    }
    false
}

/// Recursively bind mount all submounts from source to target.
///
/// This function traverses the source mount tree and creates corresponding
/// bind mounts at the target location for all submounts.
///
/// # Arguments
/// * `source_mfs` - The source MountFS to copy submounts from
/// * `_target_mfs` - The target MountFS for the recursive bind operation. The
///   current implementation derives concrete child targets from
///   `target_base_path`.
/// * `source_base_path` - The absolute path of the source mount point
/// * `target_base_path` - The absolute path of the target mount point
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
fn do_recursive_bind_mount(
    source_mfs: &Arc<MountFS>,
    _target_mfs: &Arc<MountFS>,
    source_base_path: &str,
    target_base_path: &str,
) -> Result<(), SystemError> {
    let mnt_ns = ProcessManager::current_mntns();
    let mount_list = mnt_ns.mount_list();

    // Queue for BFS traversal: (source_submount, source_mountpoint_ino)
    struct SubmountInfo {
        source_mfs: Arc<MountFS>,
        source_mp_ino: InodeId,
    }

    let mut queue: Vec<SubmountInfo> = Vec::new();

    // Add all direct submounts of source to the queue
    for (child_ino, child_mfs) in source_mfs.mountpoints().iter() {
        queue.push(SubmountInfo {
            source_mfs: child_mfs.clone(),
            source_mp_ino: *child_ino,
        });
    }

    // Process all submounts
    while let Some(info) = queue.pop() {
        // Get the mount path of this submount
        let child_mount_path = match mount_list.get_mount_path_by_ino(info.source_mp_ino) {
            Some(path) => path,
            None => {
                log::warn!(
                    "do_recursive_bind_mount: mount path not found for inode {:?}",
                    info.source_mp_ino
                );
                continue;
            }
        };

        // Calculate the relative path from source base
        let relative_path = match child_mount_path.as_str().strip_prefix(source_base_path) {
            Some(rel) => {
                if rel.is_empty() {
                    continue; // Skip if it's the source itself
                }
                rel
            }
            None => {
                log::warn!(
                    "do_recursive_bind_mount: path {} is not under source base {}",
                    child_mount_path.as_str(),
                    source_base_path
                );
                continue;
            }
        };

        // Calculate target path
        let target_child_path = if target_base_path.ends_with('/') {
            alloc::format!(
                "{}{}",
                target_base_path.trim_end_matches('/'),
                relative_path
            )
        } else {
            alloc::format!("{}{}", target_base_path, relative_path)
        };

        // log::debug!(
        //     "do_recursive_bind_mount: binding submount {} -> {}",
        //     child_mount_path.as_str(),
        //     target_child_path
        // );

        // Look up the target directory inode
        let root_inode = mnt_ns.root_inode();
        let target_child_inode = match root_inode
            .lookup_follow_symlink(&target_child_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)
        {
            Ok(inode) => inode,
            Err(e) => {
                return Err(e);
            }
        };

        let source_child_is_dir =
            info.source_mfs.root_inner_inode().metadata()?.file_type == FileType::Dir;
        let target_child_is_dir = target_child_inode.metadata()?.file_type == FileType::Dir;
        if source_child_is_dir != target_child_is_dir {
            log::warn!(
                "do_recursive_bind_mount: source and target type mismatch for {}",
                target_child_path
            );
            return Err(SystemError::ENOTDIR);
        }

        // Get the inner filesystem for mounting
        // source_mfs is already Arc<MountFS>, so we can directly call inner_filesystem()
        let child_inner_fs = info.source_mfs.inner_filesystem();

        // Create the bind mount
        let child_root_inner_inode = info.source_mfs.root_inner_inode();
        match target_child_inode
            .clone()
            .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
            .ok_or(SystemError::EINVAL)?
            .mount_subtree_with_state(
                child_inner_fs,
                child_root_inner_inode,
                MountFlags::empty(),
                Some(info.source_mfs.super_block_state()),
                Some(&info.source_mfs),
                None,
            ) {
            Ok(new_child_mnt) => {
                let source = info
                    .source_mfs
                    .mount_source()
                    .unwrap_or_else(|| String::from(child_mount_path.as_str()));
                new_child_mnt.set_mount_source(Some(source));
            }
            Err(e) => {
                log::warn!(
                    "do_recursive_bind_mount: failed to mount at {}: {:?}",
                    target_child_path,
                    e
                );
                return Err(e);
            }
        }

        // Add this submount's children to the queue
        for (grandchild_ino, grandchild_mfs) in info.source_mfs.mountpoints().iter() {
            queue.push(SubmountInfo {
                source_mfs: grandchild_mfs.clone(),
                source_mp_ino: *grandchild_ino,
            });
        }
    }

    Ok(())
}
