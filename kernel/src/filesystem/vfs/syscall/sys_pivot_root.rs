//! System call handler for pivot_root(2).

use alloc::{sync::Arc, vec::Vec};
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_PIVOT_ROOT},
    filesystem::vfs::{
        mount::MountFSInode,
        permission::PermissionMask,
        utils::{user_resolved_path_at, ResolvedPath},
        FileType, IndexNode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::{all_process, lock_fs_refs_tasklist, ProcessControlBlock, ProcessManager},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
    },
};

use super::sys_mount::may_mount;

pub struct SysPivotRootHandle;

struct PivotRootTargets {
    current_mntns: Arc<crate::process::namespace::mnt::MntNamespace>,
    current_root: ResolvedPath,
    new_root: ResolvedPath,
    put_old: ResolvedPath,
}

impl Syscall for SysPivotRootHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let targets = resolve_pivot_root_targets(Self::new_root(args), Self::put_old(args))?;
        let old_root = inode_as_mountpoint(&targets.current_root.inode())?;
        let new_root = inode_as_mountpoint(&targets.new_root.inode())?;
        let put_old = inode_as_mountpoint(&targets.put_old.inode())?;

        // Match Linux's tasklist_lock boundary around chroot_fs_refs(): a
        // newly copied fs_struct cannot become visible between this fixed
        // published-task snapshot and the exact-reference migration.
        let _fs_refs_tasklist = lock_fs_refs_tasklist();
        let tasks = snapshot_all_processes()?;
        let mut changed_fs = HashMap::new();
        changed_fs
            .try_reserve(tasks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let mut refresh_tasks = Vec::new();
        refresh_tasks
            .try_reserve(tasks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let topology = targets
            .current_mntns
            .pivot_root(old_root, new_root, put_old)?;
        repair_fs_refs(
            &tasks,
            &targets.current_root,
            &targets.new_root,
            &mut changed_fs,
            &mut refresh_tasks,
        );
        drop(topology);
        drop(_fs_refs_tasklist);
        refresh_cwd_caches(&refresh_tasks);

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
    let (new_root_begin, new_root_rest) = user_resolved_path_at(
        &current_pcb,
        crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
        &new_root_path,
    )?;
    let new_root = new_root_begin.inode().lookup_follow_symlink_owned(
        &new_root_begin,
        &new_root_rest,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
        true,
    )?;

    let (put_old_begin, put_old_rest) = user_resolved_path_at(
        &current_pcb,
        crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
        &put_old_path,
    )?;
    let put_old = put_old_begin.inode().lookup_follow_symlink_owned(
        &put_old_begin,
        &put_old_rest,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
        true,
    )?;
    // Match Linux get_fs_root(): one pinned caller-root snapshot is used for
    // every check, topology mutation, and chroot_fs_refs-style replacement.
    let current_root = current_pcb.fs_struct().root_resolved()?;

    ensure_searchable_dir(&new_root.inode())?;
    ensure_searchable_dir(&put_old.inode())?;

    Ok(PivotRootTargets {
        current_mntns,
        current_root,
        new_root,
        put_old,
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

fn inode_as_mountpoint(inode: &Arc<dyn IndexNode>) -> Result<Arc<MountFSInode>, SystemError> {
    inode
        .clone()
        .downcast_arc::<MountFSInode>()
        .ok_or(SystemError::EINVAL)
}

fn snapshot_all_processes() -> Result<Vec<Arc<ProcessControlBlock>>, SystemError> {
    let mut tasks = Vec::new();
    loop {
        let all = all_process().lock_irqsave();
        let required = all.as_ref().map(|map| map.len()).unwrap_or(0);
        if tasks.capacity() >= required {
            if let Some(map) = all.as_ref() {
                // Capacity was reserved outside the IRQ-disabled section, so
                // cloning the published PCB Arcs here cannot allocate.
                tasks.extend(map.values().cloned());
            }
            return Ok(tasks);
        }
        drop(all);
        tasks
            .try_reserve(required)
            .map_err(|_| SystemError::ENOMEM)?;
    }
}

fn repair_fs_refs(
    tasks: &[Arc<ProcessControlBlock>],
    old_root: &ResolvedPath,
    new_root: &ResolvedPath,
    changed_fs: &mut HashMap<usize, Arc<crate::filesystem::fs::FsStruct>>,
    refresh_tasks: &mut Vec<Arc<ProcessControlBlock>>,
) {
    for task in tasks {
        let _slot_update = task.lock_fs_slot_update();
        let Some(fs) = task.try_fs_struct() else {
            continue;
        };
        let fs_id = Arc::as_ptr(&fs) as usize;
        if fs.replace_root_pwd(old_root, new_root) {
            // Retain the owner as well as indexing by address. This prevents
            // allocator address reuse from turning the grouping key into an
            // ABA match while another task concurrently replaces its slot.
            changed_fs.insert(fs_id, fs.clone());
        }
        // Every PCB sharing a changed FsStruct owns an independent display
        // cache, even though only the first exact replacement reports a hit.
        if changed_fs.contains_key(&fs_id) {
            refresh_tasks.push(task.clone());
        }
    }
}

fn refresh_cwd_caches(tasks: &[Arc<ProcessControlBlock>]) {
    for task in tasks {
        let Some(fs) = task.try_fs_struct() else {
            continue;
        };
        // basic.cwd is only a display cache. An unlinked cwd can make path
        // rendering fail after commit; cache refresh must not change success.
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
