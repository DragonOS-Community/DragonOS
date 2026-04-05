//! System call handler for pivot_root(2).

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_PIVOT_ROOT},
    filesystem::vfs::{
        mount::MountFSInode,
        permission::PermissionMask,
        utils::{is_ancestor, user_path_at},
        FileSystem, FileType, IndexNode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::{all_process, cred::CAPFlags, ProcessControlBlock, ProcessManager},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
    },
};

pub struct SysPivotRootHandle;

struct PivotRootTargets {
    current_pcb: Arc<ProcessControlBlock>,
    current_mntns: Arc<crate::process::namespace::mnt::MntNamespace>,
    new_root_mntfs: Arc<crate::filesystem::vfs::MountFS>,
    put_old_mountpoint: Arc<MountFSInode>,
    old_new_root_path: String,
    old_put_old_path: String,
    new_put_old_path: String,
}

impl Syscall for SysPivotRootHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let targets = resolve_pivot_root_targets(Self::new_root(args), Self::put_old(args))?;
        let current_task = targets.current_pcb.clone();
        let old_root_inode = targets.current_mntns.root_inode();

        targets.current_mntns.pivot_root(
            targets.new_root_mntfs.clone(),
            targets.put_old_mountpoint.clone(),
            &targets.old_new_root_path,
            &targets.old_put_old_path,
            &targets.new_put_old_path,
        )?;

        let new_root_inode = targets.current_mntns.root_inode();
        repair_same_namespace_fs_refs(
            &targets.current_mntns,
            &current_task,
            &old_root_inode,
            &new_root_inode,
            &targets.old_new_root_path,
            &targets.new_put_old_path,
        )?;

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
    if !current_pcb.cred().has_capability(CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EPERM);
    }

    let current_mntns = ProcessManager::current_mntns();
    let namespace_root_inode = current_mntns.root_inode();
    let current_root_inode = namespace_root_inode.clone();

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

    if !is_ancestor(&current_root_inode, &new_root_inode) {
        return Err(SystemError::EINVAL);
    }

    if !is_ancestor(&new_root_inode, &put_old_inode) {
        return Err(SystemError::EINVAL);
    }

    let new_root_parent = new_root_mntfs
        .self_mountpoint()
        .ok_or(SystemError::EINVAL)?;
    let new_root_parent_mntfs = new_root_parent.mount_fs();

    if current_root_mntfs.propagation().is_shared()
        || new_root_mntfs.propagation().is_shared()
        || new_root_parent_mntfs.propagation().is_shared()
        || put_old_mntfs.propagation().is_shared()
    {
        return Err(SystemError::EINVAL);
    }

    let old_new_root_path = new_root_inode.absolute_path()?;
    let put_old_path_before = put_old_inode.absolute_path()?;
    let new_put_old_path = put_old_path_before
        .strip_prefix(&old_new_root_path)
        .map(normalize_visible_path)
        .ok_or(SystemError::EINVAL)?;

    Ok(PivotRootTargets {
        current_pcb,
        current_mntns,
        new_root_mntfs,
        put_old_mountpoint,
        old_new_root_path,
        old_put_old_path: put_old_path_before,
        new_put_old_path,
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
    let left_meta = match left.metadata() {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    let right_meta = match right.metadata() {
        Ok(meta) => meta,
        Err(_) => return false,
    };

    Arc::ptr_eq(&left.fs(), &right.fs()) && left_meta.inode_id == right_meta.inode_id
}

fn repair_same_namespace_fs_refs(
    target_mntns: &Arc<crate::process::namespace::mnt::MntNamespace>,
    current_task: &Arc<ProcessControlBlock>,
    old_root_inode: &Arc<dyn IndexNode>,
    new_root_inode: &Arc<dyn IndexNode>,
    old_new_root_path: &str,
    new_put_old_path: &str,
) -> Result<(), SystemError> {
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

        let fs = task.fs_struct();
        let is_current_task = Arc::ptr_eq(&task, current_task);
        let root_replaced = same_path_ref(&fs.root(), old_root_inode);
        let pwd_replaced = same_path_ref(&fs.pwd(), old_root_inode);
        let rewrite_cwd = is_current_task || root_replaced || pwd_replaced;

        if !root_replaced && !pwd_replaced && !rewrite_cwd {
            continue;
        }

        let old_cwd = task.basic().cwd();
        let mut basic = task.basic_mut();
        let fs_guard = task.fs_struct_mut();

        if root_replaced || is_current_task {
            fs_guard.set_root(new_root_inode.clone());
        }
        if pwd_replaced {
            fs_guard.set_pwd(new_root_inode.clone());
        }

        if rewrite_cwd {
            let new_cwd = if pwd_replaced {
                "/".to_string()
            } else {
                rewrite_visible_path(&old_cwd, old_new_root_path, new_put_old_path)
            };
            basic.set_cwd(new_cwd);
        }
    }

    Ok(())
}

fn normalize_visible_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

fn rewrite_visible_path(path: &str, old_new_root_path: &str, new_put_old_path: &str) -> String {
    if path == old_new_root_path {
        return "/".to_string();
    }

    if let Some(suffix) = path.strip_prefix(old_new_root_path) {
        if suffix.is_empty() {
            return "/".to_string();
        }
        return normalize_visible_path(suffix);
    }

    if path == "/" {
        return new_put_old_path.to_string();
    }

    if new_put_old_path == "/" {
        return normalize_visible_path(path);
    }

    let mut result = new_put_old_path.trim_end_matches('/').to_string();
    result.push('/');
    result.push_str(path.trim_start_matches('/'));
    result
}
