//! System call handler for sys_mount.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MOUNT},
    filesystem::vfs::{
        fcntl::AtFlags,
        mount::{is_mountpoint_root, MountFlags},
        produce_fs,
        utils::user_path_at,
        FileType, FsReconfigureRequest, IndexNode, InodeId, MountFS, MAX_PATHLEN,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
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

    // MS_REMOUNT|MS_BIND 和 MS_REMOUNT 共用此 atime 保留逻辑。
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
        log::warn!("todo: move mnt");
        return Err(SystemError::ENOSYS);
    }

    // 创建新的挂载
    return do_new_mount(source, target_inode, filesystemtype, data, mnt_flags).map(|_| ());
}

/// 修改已有挂载的 mount flags
///
/// Linux 两条独立路径：
/// - do_reconfigure_mnt()：MS_REMOUNT|MS_BIND, down_read(sb), 只改 mount flags ← 本函数
/// - do_remount()：MS_REMOUNT alone, down_write(sb) + reconfigure_super() + set_mount_attributes() ← TODO
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

    // 目标 mount 必须属于当前进程的 mount namespace。
    let current_mntns = ProcessManager::current_mntns();
    if !target_mfs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }

    // 保留不可修改的 flags，只覆盖 SETTABLE 位。
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
    mut target_inode: Arc<dyn IndexNode>,
    filesystemtype: Option<String>,
    data: Option<String>,
    mount_flags: MountFlags,
) -> Result<Arc<MountFS>, SystemError> {
    let fs_type_str = filesystemtype.ok_or(SystemError::EINVAL)?;
    let source = source.ok_or(SystemError::EINVAL)?;
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
    let new_mount = target_inode.mount(fs, mount_flags)?;
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

    let source_is_dir = source_inode.metadata()?.file_type == FileType::Dir;
    let target_is_dir = target_inode.metadata()?.file_type == FileType::Dir;
    if source_is_dir != target_is_dir {
        return Err(SystemError::ENOTDIR);
    }

    // Get the source's filesystem
    let source_fs = source_inode.fs();

    // Check if source is on a MountFS
    let source_mfs = source_fs.clone().downcast_arc::<MountFS>();

    // 源挂载必须属于当前 mount namespace。
    if let Some(ref mfs) = source_mfs {
        let current_mntns = ProcessManager::current_mntns();
        if !mfs.is_belongs_to_mntns(&current_mntns) {
            return Err(SystemError::EINVAL);
        }
    }

    // Check if source is unbindable - if so, reject the bind mount
    if let Some(ref mfs) = source_mfs {
        if mfs.propagation().is_unbindable() {
            // log::debug!("do_bind_mount: source is unbindable, rejecting bind mount");
            return Err(SystemError::EINVAL);
        }
    }

    // Clone source_mfs for recursive bind mount (need to keep it for later use)
    let source_mfs_for_recursive = source_mfs.clone();

    let root_inner_inode = source_inode
        .clone()
        .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
        .map(|inode| inode.underlying_inode())
        .unwrap_or_else(|| source_inode.clone());

    // Get the inner filesystem for mounting while preserving the source subtree root.
    let inner_fs = source_mfs
        .map(|mfs| mfs.inner_filesystem())
        .unwrap_or(source_fs);

    // do_loopback：目标挂载点必须属于当前 mount namespace。
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
        )?;
    target_mfs.set_mount_source(Some(source_path.clone()));

    // If MS_REC is set, recursively bind all submounts from source to target
    if flags.contains(MountFlags::REC) {
        if let Some(ref mfs) = source_mfs_for_recursive {
            // Linux kern_path() 将用户路径解析为 struct path，后续 copy_tree 基于
            // 内核 mount/dentry 数据结构遍历子挂载，不涉及字符串路径匹配。
            // DragonOS 使用 strip_prefix 做路径匹配，因此必须将 source_path 规范化为
            // 与 mount_list 存储格式（absolute_path 产生的规范化绝对路径）一致。
            // 直接传用户原始字符串会在相对路径、含 .. 的路径、符号链接路径下导致
            // strip_prefix 匹配失败，静默跳过所有子挂载。
            let resolved_source_path = match source_inode.absolute_path() {
                Ok(p) => p,
                Err(_) => {
                    // absolute_path 失败（如 devfs 设备节点）。
                    // 文件型 bind mount 没有子挂载，跳过递归是安全的。
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
                // Linux copy_tree 失败时调用 umount_tree(res, UMOUNT_SYNC) 递归回滚整棵子树。
                // Linux do_loopback 中 graft_tree 失败时同样调用 umount_tree 回滚。
                // 保证 all-or-nothing 原子语义。
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
/// namespace 隔离由 VFS 路径查找隐式保证。
fn do_change_type(target_inode: Arc<dyn IndexNode>, flags: MountFlags) -> Result<(), SystemError> {
    // 目标必须是挂载点根
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
