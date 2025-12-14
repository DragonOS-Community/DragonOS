//! System call handler for sys_mount.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MOUNT},
    filesystem::vfs::{
        fcntl::AtFlags,
        mount::{is_mountpoint_root, MountFlags},
        produce_fs,
        utils::user_path_at,
        FileType, IndexNode, InodeId, MountFS, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::{
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

/// #挂载文件系统
///
/// 用于挂载文件系统,目前仅支持ramfs挂载
///
/// ## 参数:
///
/// - source       挂载设备(目前只支持ext4格式的硬盘)
/// - target       挂载目录
/// - filesystemtype   文件系统
/// - mountflags     挂载选项
/// - data        带数据挂载
///
/// ## 返回值
/// - Ok(0): 挂载成功
/// - Err(SystemError) :挂载过程中出错
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

        let target = copy_mount_string(target).inspect_err(|e| {
            log::error!("Failed to read mount target: {:?}", e);
        })?;
        let source = copy_mount_string(source).inspect_err(|e| {
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

/// # do_mount - 挂载文件系统
///
/// 将给定的文件系统挂载到指定的挂载点。
///
/// 此函数会检查是否已经挂载了相同的文件系统，如果已经挂载，则返回错误。
/// 它还会处理符号链接，并确保挂载点是有效的。
///
/// ## 参数
///
/// - `fs`: Arc<dyn FileSystem>，要挂载的文件系统。
/// - `mount_point`: &str，挂载点路径。
///
/// ## 返回值
///
/// - `Ok(Arc<MountFS>)`: 挂载成功后返回挂载的文件系统。
/// - `Err(SystemError)`: 挂载失败时返回错误。
pub fn do_mount(
    source: Option<String>,
    target: Option<String>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mount_flags: MountFlags,
) -> Result<(), SystemError> {
    let (current_node, rest_path) = user_path_at(
        &ProcessManager::current_pcb(),
        AtFlags::AT_FDCWD.bits(),
        target.as_deref().unwrap_or(""),
    )?;
    let inode = current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    return path_mount(source, inode, filesystemtype, data, mount_flags);
}

fn path_mount(
    source: Option<String>,
    target_inode: Arc<dyn IndexNode>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mut flags: MountFlags,
) -> Result<(), SystemError> {
    let mut mnt_flags = MountFlags::empty();

    if flags & MountFlags::MGC_MASK == MountFlags::MGC_VAL {
        flags.remove(MountFlags::MGC_MASK);
    }

    if flags.contains(MountFlags::NOUSER) {
        return Err(SystemError::EINVAL);
    }

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

    // todo: 处理remount时，atime相关的选项
    // https://code.dragonos.org.cn/xref/linux-6.6.21/fs/namespace.c#3646

    // todo: 参考linux的，实现对各个挂载选项的处理
    // https://code.dragonos.org.cn/xref/linux-6.6.21/fs/namespace.c#3662
    if flags.intersection(MountFlags::REMOUNT | MountFlags::BIND)
        == (MountFlags::REMOUNT | MountFlags::BIND)
    {
        log::warn!("todo: reconfigure mnt");
        return Err(SystemError::ENOSYS);
    }

    if flags.contains(MountFlags::REMOUNT) {
        log::warn!("todo: remount");
        return Err(SystemError::ENOSYS);
    }

    if flags.contains(MountFlags::BIND) {
        return do_bind_mount(source, target_inode, flags);
    }
    // Handle propagation type changes (mount --make-{shared,private,slave,unbindable})
    if is_propagation_change(flags) {
        return do_change_type(target_inode, flags);
    }

    if flags.contains(MountFlags::MOVE) {
        log::warn!("todo: move mnt");
        return Err(SystemError::ENOSYS);
    }

    // 创建新的挂载
    return do_new_mount(source, target_inode, filesystemtype, data, mnt_flags).map(|_| ());
}

fn do_new_mount(
    source: Option<String>,
    mut target_inode: Arc<dyn IndexNode>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mount_flags: MountFlags,
) -> Result<Arc<MountFS>, SystemError> {
    let fs_type_str = filesystemtype.ok_or(SystemError::EINVAL)?;
    let source = source.ok_or(SystemError::EINVAL)?;
    let fs = produce_fs(&fs_type_str, data.as_deref(), &source).inspect_err(|e| {
        log::error!("Failed to produce filesystem: {:?}", e);
    })?;

    // 若目标是挂载点根，则尝试在其父目录挂载，避免 EBUSY 并与 Linux 叠加语义接近
    if is_mountpoint_root(&target_inode) {
        if let Ok(parent) = target_inode.parent() {
            target_inode = parent;
        }
    }

    let _abs_path = target_inode.absolute_path()?;

    // 允许在已有挂载点上再次挂载（符合 Linux 允许叠加挂载的语义）
    // MountList::insert 会替换同一路径的记录，无需提前返回 EBUSY。
    return target_inode.mount(fs, mount_flags);
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

/// Perform a bind mount operation.
///
/// Bind mount makes a directory subtree visible at another location.
/// The source and target will share the same underlying filesystem content.
///
/// # Arguments
/// * `source` - The source path to bind from
/// * `target_inode` - The target mount point
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

    // Both source and target must be directories
    if source_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    if target_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // Get the source's filesystem
    let source_fs = source_inode.fs();

    // Check if source is on a MountFS
    let source_mfs = source_fs.clone().downcast_arc::<MountFS>();

    // Check if source is unbindable - if so, reject the bind mount
    if let Some(ref mfs) = source_mfs {
        if mfs.propagation().is_unbindable() {
            // log::debug!("do_bind_mount: source is unbindable, rejecting bind mount");
            return Err(SystemError::EINVAL);
        }
    }

    // Clone source_mfs for recursive bind mount (need to keep it for later use)
    let source_mfs_for_recursive = source_mfs.clone();

    // Get the inner filesystem for mounting
    let inner_fs = source_mfs
        .map(|mfs| mfs.inner_filesystem())
        .unwrap_or(source_fs);

    // Use the target_inode.mount() method which handles all the mounting logic
    // This properly creates a new MountFS and registers it
    let target_mfs = target_inode.mount(inner_fs, MountFlags::empty())?;

    // If MS_REC is set, recursively bind all submounts from source to target
    if flags.contains(MountFlags::REC) {
        if let Some(ref mfs) = source_mfs_for_recursive {
            let source_path = source_inode.absolute_path()?;
            let target_path = target_inode.absolute_path()?;
            do_recursive_bind_mount(mfs, &target_mfs, &source_path, &target_path)?;
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
fn do_change_type(target_inode: Arc<dyn IndexNode>, flags: MountFlags) -> Result<(), SystemError> {
    // Get the propagation type from flags
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
    let fs = target_inode.fs();
    let mount_fs = fs.downcast_arc::<MountFS>().ok_or_else(|| {
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

/// Recursively bind mount all submounts from source to target.
///
/// This function traverses the source mount tree and creates corresponding
/// bind mounts at the target location for all submounts.
///
/// # Arguments
/// * `source_mfs` - The source MountFS to copy submounts from
/// * `target_mfs` - The target MountFS to create submounts in
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
                log::warn!(
                    "do_recursive_bind_mount: failed to lookup target path {}: {:?}",
                    target_child_path,
                    e
                );
                continue;
            }
        };

        // Skip if not a directory
        if target_child_inode.metadata()?.file_type != FileType::Dir {
            log::warn!(
                "do_recursive_bind_mount: target {} is not a directory",
                target_child_path
            );
            continue;
        }

        // Get the inner filesystem for mounting
        // source_mfs is already Arc<MountFS>, so we can directly call inner_filesystem()
        let child_inner_fs = info.source_mfs.inner_filesystem();

        // Create the bind mount
        if let Err(e) = target_child_inode.mount(child_inner_fs, MountFlags::empty()) {
            log::warn!(
                "do_recursive_bind_mount: failed to mount at {}: {:?}",
                target_child_path,
                e
            );
            continue;
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
