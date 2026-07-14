//! System call handler for pivot_root(2).

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_PIVOT_ROOT},
    filesystem::vfs::{
        mount::MountFSInode, permission::PermissionMask, utils::user_path_at, FileSystem, FileType,
        IndexNode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::{all_process, ProcessControlBlock, ProcessManager},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
    },
};

use super::sys_mount::may_mount;

pub struct SysPivotRootHandle;

struct PivotRootTargets {
    current_pcb: Arc<ProcessControlBlock>,
    current_mntns: Arc<crate::process::namespace::mnt::MntNamespace>,
    current_root_mntfs: Arc<crate::filesystem::vfs::MountFS>,
    new_root_mntfs: Arc<crate::filesystem::vfs::MountFS>,
    put_old_mountpoint: Arc<MountFSInode>,
}

impl Syscall for SysPivotRootHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let targets = resolve_pivot_root_targets(Self::new_root(args), Self::put_old(args))?;
        let old_root_inode = targets.current_pcb.fs_struct().root();

        targets.current_mntns.pivot_root(
            targets.current_root_mntfs.clone(),
            targets.new_root_mntfs.clone(),
            targets.put_old_mountpoint.clone(),
        )?;

        // pivot_root operates on the caller's fs root, which may be a nested
        // mount after chroot(2); it does not necessarily replace the mount
        // namespace's global root.
        let new_root_inode = targets.new_root_mntfs.root_inode();
        repair_same_namespace_fs_refs(&targets.current_mntns, &old_root_inode, &new_root_inode);

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("new_root", format!("{:#x}", Self::new_root(args) as usize)),
            FormattedSyscallParam::new("put_old", format!("{:#x}", Self::put_old(args) as usize)),
        ]
    }
}

impl SysPivotRootHandle {
    fn new_root(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn put_old(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_PIVOT_ROOT, SysPivotRootHandle);

fn resolve_pivot_root_targets(
    new_root_ptr: *const u8,
    put_old_ptr: *const u8,
) -> Result<PivotRootTargets, SystemError> {
    if !may_mount() {
        return Err(SystemError::EPERM);
    }

    if new_root_ptr.is_null() || put_old_ptr.is_null() {
        return Err(SystemError::EFAULT);
    }

    let new_root_path = vfs_check_and_clone_cstr(new_root_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let put_old_path = vfs_check_and_clone_cstr(put_old_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    if new_root_path.is_empty() || put_old_path.is_empty() {
        return Err(SystemError::ENOENT);
    }

    let current_pcb = ProcessManager::current_pcb();
    let current_mntns = ProcessManager::current_mntns();
    let current_root_inode = current_pcb.fs_struct().root();

    let (new_root_begin, new_root_rest) = user_path_at(
        &current_pcb,
        crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
        &new_root_path,
    )?;
    let new_root_inode =
        new_root_begin.lookup_follow_symlink(&new_root_rest, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    let (put_old_begin, put_old_rest) = user_path_at(
        &current_pcb,
        crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
        &put_old_path,
    )?;
    let put_old_inode =
        put_old_begin.lookup_follow_symlink(&put_old_rest, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    ensure_searchable_dir(&new_root_inode)?;
    ensure_searchable_dir(&put_old_inode)?;

    let current_root_mntfs = mount_fs_from_inode(&current_root_inode)?;
    let new_root_mountpoint = inode_as_mountpoint(&new_root_inode)?;
    let put_old_mountpoint = inode_as_mountpoint(&put_old_inode)?;
    if new_root_mountpoint.is_disconnected() {
        return Err(SystemError::ENOENT);
    }
    let new_root_mntfs = new_root_mountpoint.mount_fs();
    let put_old_mntfs = put_old_mountpoint.mount_fs();

    if same_path_ref(&new_root_inode, &current_root_inode)
        || same_path_ref(&put_old_inode, &current_root_inode)
    {
        return Err(SystemError::EBUSY);
    }

    if !is_mount_root(&new_root_inode)? {
        return Err(SystemError::EINVAL);
    }

    // Linux requires the caller's current fs root itself to be a mounted
    // path. A chroot into an ordinary subdirectory must not rotate the whole
    // containing mount.
    if !is_mount_root(&current_root_inode)? {
        return Err(SystemError::EINVAL);
    }

    if !is_path_reachable(&current_root_inode, &new_root_inode)? {
        return Err(SystemError::EINVAL);
    }

    if !is_path_reachable(&new_root_inode, &put_old_inode)? {
        return Err(SystemError::EINVAL);
    }

    let new_root_parent = new_root_mntfs
        .self_mountpoint()
        .ok_or(SystemError::EINVAL)?;
    let new_root_parent_mntfs = new_root_parent.mount_fs();

    let current_root_parent_mntfs = current_root_mntfs
        .self_mountpoint()
        .map(|mountpoint| mountpoint.mount_fs())
        .unwrap_or_else(|| current_root_mntfs.clone());

    if current_root_parent_mntfs.propagation().is_shared()
        || new_root_parent_mntfs.propagation().is_shared()
        || put_old_mntfs.propagation().is_shared()
        || new_root_mntfs.is_locked()
    {
        return Err(SystemError::EINVAL);
    }

    Ok(PivotRootTargets {
        current_pcb,
        current_mntns,
        current_root_mntfs,
        new_root_mntfs,
        put_old_mountpoint,
    })
}

fn ensure_searchable_dir(inode: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
    let metadata = inode.metadata()?;
    if metadata.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    crate::filesystem::vfs::permission::check_inode_permission(
        inode,
        &metadata,
        PermissionMask::MAY_EXEC,
    )
}

fn mount_fs_from_inode(
    inode: &Arc<dyn IndexNode>,
) -> Result<Arc<crate::filesystem::vfs::MountFS>, SystemError> {
    inode
        .fs()
        .downcast_arc::<crate::filesystem::vfs::MountFS>()
        .ok_or(SystemError::EINVAL)
}

fn inode_as_mountpoint(inode: &Arc<dyn IndexNode>) -> Result<Arc<MountFSInode>, SystemError> {
    inode
        .clone()
        .downcast_arc::<MountFSInode>()
        .ok_or(SystemError::EINVAL)
}

fn is_mount_root(inode: &Arc<dyn IndexNode>) -> Result<bool, SystemError> {
    let mount_fs = mount_fs_from_inode(inode)?;
    let mount_root = mount_fs.root_inode();
    Ok(same_path_ref(inode, &mount_root))
}

fn same_path_ref(left: &Arc<dyn IndexNode>, right: &Arc<dyn IndexNode>) -> bool {
    let Some(left) = left.clone().downcast_arc::<MountFSInode>() else {
        return Arc::ptr_eq(left, right);
    };
    let Some(right) = right.clone().downcast_arc::<MountFSInode>() else {
        return false;
    };
    Arc::ptr_eq(&left.mount_fs(), &right.mount_fs()) && left.dentry_id() == right.dentry_id()
}

fn is_path_reachable(
    ancestor: &Arc<dyn IndexNode>,
    descendant: &Arc<dyn IndexNode>,
) -> Result<bool, SystemError> {
    let mut current = descendant.clone();
    for _ in 0..=MAX_PATHLEN {
        if same_path_ref(ancestor, &current) {
            return Ok(true);
        }
        let parent = current.parent()?;
        if same_path_ref(&parent, &current) {
            return Ok(false);
        }
        current = parent;
    }
    Err(SystemError::ELOOP)
}

fn repair_same_namespace_fs_refs(
    target_mntns: &Arc<crate::process::namespace::mnt::MntNamespace>,
    old_root_inode: &Arc<dyn IndexNode>,
    new_root_inode: &Arc<dyn IndexNode>,
) {
    let tasks: Vec<Arc<ProcessControlBlock>> = {
        let all = all_process().lock_irqsave();
        all.as_ref()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default()
    };

    for task in tasks {
        if !Arc::ptr_eq(task.nsproxy().mnt_namespace(), target_mntns) {
            continue;
        }

        let Some(fs) = task.try_fs_struct() else {
            continue;
        };
        let root_replaced = same_path_ref(&fs.root(), old_root_inode);
        let pwd_replaced = same_path_ref(&fs.pwd(), old_root_inode);
        if !root_replaced && !pwd_replaced {
            continue;
        }

        if root_replaced {
            fs.set_root(new_root_inode.clone());
        }
        if pwd_replaced {
            fs.set_pwd(new_root_inode.clone());
        }
        // basic.cwd is only a display cache.  An unlinked cwd can make path
        // rendering fail after the topology commit; Linux's chroot_fs_refs()
        // is infallible, so cache refresh must not change syscall success.
        if let Ok(cwd) = visible_pwd(&fs) {
            task.basic_mut().set_cwd(cwd);
        }
    }
}

fn visible_pwd(
    fs: &Arc<crate::filesystem::fs::FsStruct>,
) -> Result<alloc::string::String, SystemError> {
    let root = fs.root().absolute_path()?;
    let pwd = fs.pwd().absolute_path()?;
    if pwd == root {
        return Ok("/".into());
    }
    if root == "/" {
        return Ok(pwd);
    }
    let prefix = format!("{}/", root.trim_end_matches('/'));
    let relative = pwd.strip_prefix(&prefix).ok_or(SystemError::ENOENT)?;
    Ok(format!("/{relative}"))
}
