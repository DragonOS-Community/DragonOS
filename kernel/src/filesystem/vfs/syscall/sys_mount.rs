//! System call handler for sys_mount.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MOUNT},
    filesystem::vfs::{
        fcntl::AtFlags,
        mount::{is_mountpoint_root, MountFSInode, MountFlags},
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
use alloc::string::ToString;
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
    // log::info!(
    //     "[do_mount] source={:?}, target={:?}, filesystemtype={:?}, flags={:?}",
    //     source,
    //     target,
    //     filesystemtype,
    //     mount_flags
    // );
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
        // MS_REMOUNT | MS_BIND: 修改已存在挂载的标志，不创建新挂载
        return do_reconfigure_mnt(target_inode, mnt_flags);
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

/// 处理 MS_REMOUNT | MS_BIND 情况
///
/// 这用于修改已存在挂载的挂载标志（如只读状态），而不改变挂载本身。
/// 参考 Linux 的 do_reconfigure_mnt 实现。
fn do_reconfigure_mnt(
    target_inode: Arc<dyn IndexNode>,
    new_flags: MountFlags,
) -> Result<(), SystemError> {
    use crate::filesystem::vfs::mount::MountFSInode;

    log::debug!("[do_reconfigure_mnt] new_flags={:?}", new_flags);

    // 获取目标 inode 对应的 MountFS
    let mount_fs =
        if let Some(mountfs_inode) = target_inode.as_any_ref().downcast_ref::<MountFSInode>() {
            mountfs_inode.mount_fs().clone()
        } else {
            // 如果不是 MountFSInode，尝试通过文件系统查找
            let mnt_ns = ProcessManager::current_mntns();
            let inode_fs = target_inode.fs();

            // 尝试通过文件系统查找对应的 MountFS
            if let Some(mount_fs) = mnt_ns.mount_list().find_mount_by_fs(&inode_fs) {
                mount_fs
            } else {
                return Err(SystemError::EINVAL);
            }
        };

    // 修改挂载标志
    // 注意：我们保留一些不应该被修改的标志（如 propagation 相关的）
    let current_flags = mount_fs.mount_flags();

    // 保留 propagation 标志（SHARED, PRIVATE, SLAVE, UNBINDABLE）
    let propagation_flags =
        MountFlags::SHARED | MountFlags::PRIVATE | MountFlags::SLAVE | MountFlags::UNBINDABLE;
    let current_prop = current_flags & propagation_flags;

    // 合并新的标志和保留的 propagation 标志
    let merged_flags = new_flags | current_prop;

    log::debug!(
        "[do_reconfigure_mnt] current_flags={:?}, new_flags={:?}, merged_flags={:?}",
        current_flags,
        new_flags,
        merged_flags
    );

    mount_fs.set_mount_flags(merged_flags);

    Ok(())
}

fn do_new_mount(
    source: Option<String>,
    mut target_inode: Arc<dyn IndexNode>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mount_flags: MountFlags,
) -> Result<Arc<MountFS>, SystemError> {
    let _target_path = target_inode
        .absolute_path()
        .unwrap_or_else(|_| "?".to_string());
    let fs_type_str = filesystemtype.ok_or(SystemError::EINVAL)?;
    let source = source.ok_or(SystemError::EINVAL)?;
    // log::info!(
    //     "[do_new_mount] source={}, fs_type={}, target={}, flags={:?}",
    //     source,
    //     fs_type_str,
    //     target_path,
    //     mount_flags
    // );
    let fs = produce_fs(&fs_type_str, data.as_deref(), &source).inspect_err(|e| {
        log::warn!("Failed to produce filesystem: {:?}", e);
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
    log::debug!(
        "[do_bind_mount] current_node fs={}, rest_path={}",
        current_node.fs().name(),
        rest_path
    );
    let source_inode =
        current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    log::debug!(
        "[do_bind_mount] source_inode fs={}",
        source_inode.fs().name()
    );

    // Both source and target must be directories
    if source_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    if target_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // Get the source's filesystem
    let source_fs = source_inode.fs();
    let _source_path_res = source_inode
        .absolute_path()
        .unwrap_or_else(|_| source_path.clone());

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
        .unwrap_or(source_fs.clone());

    // Use the target_inode.mount() method which handles all the mounting logic
    // This properly creates a new MountFS and registers it
    // log::info!(
    //     "[do_bind_mount] source_path={:?}, source_fs={}, target_path={:?}",
    //     source_path_res,
    //     source_fs.name(),
    //     target_inode
    //         .absolute_path()
    //         .unwrap_or_else(|_| "?".to_string())
    // );
    let target_mfs = target_inode.mount(inner_fs, MountFlags::empty())?;
    // log::info!(
    //     "[do_bind_mount] created MountFS id={:?}, fs={}",
    //     target_mfs.mount_id(),
    //     target_mfs.fs_type()
    // );

    // 设置 bind_target_root
    // DragonOS 的 bind mount 与 Linux 有差异：
    // - Linux: bind mount 创建的挂载以 bind target 目录为根
    // - DragonOS: bind mount 包装整个底层文件系统，MountFS::root_inode() 返回底层文件系统的根
    //
    // 为了支持容器场景，我们需要告诉 MountFS：当这个 mount 被用作根文件系统时，
    // 应该返回 bind target 目录的内容，而不是底层文件系统的根。
    //
    // 我们创建一个 MountFSInode 来包装 source_inode（bind target 目录），
    // 并将其设置为 target_mfs 的 bind_target_root。
    let bind_target_root_inode = MountFSInode::new(source_inode.clone(), target_mfs.clone());
    target_mfs.set_bind_target_root(bind_target_root_inode);
    // log::info!(
    //     "[do_bind_mount] set bind_target_root for MountFS id={:?}",
    //     target_mfs.mount_id()
    // );

    // 特殊处理：如果 bind 挂载到 "/"，需要更新 namespace 的根
    let target_path = target_inode
        .absolute_path()
        .unwrap_or_else(|_| "?".to_string());
    if target_path == "/" {
        // log::info!("[do_bind_mount] binding to root, updating namespace root");
        let mnt_ns = ProcessManager::current_mntns();
        unsafe {
            // 这会替换 namespace 的根挂载点
            mnt_ns.force_change_root_mountfs(target_mfs.clone());
        }
        // 同时更新当前进程的根目录
        let pcb = ProcessManager::current_pcb();
        pcb.fs_struct_mut().set_root(mnt_ns.root_inode());
    }

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
