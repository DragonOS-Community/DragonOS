//! System call handler for pivot_root(2).
//!
//! Linux 语义要点：
//! - pivot_root() 将当前进程的根文件系统切换到 new_root，并将原根文件系统挂载到 put_old
//! - new_root 和 put_old 必须在不同的挂载点
//! - put_old 必须在 new_root 之下
//! - new_root 和 put_old 都必须是目录
//! - 当前工作目录不能在 put_old 中
//! - 需要 CAP_SYS_CHROOT 权限
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PIVOT_ROOT;
use crate::filesystem::vfs::mount::{MountFS, MountFSInode, MountPath};
use crate::filesystem::vfs::permission::PermissionMask;
use crate::filesystem::vfs::{
    utils::{is_ancestor_limited, user_path_at},
    FileType, IndexNode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::process::cred::CAPFlags;
use crate::process::namespace::mnt::MntNamespace;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysPivotRootHandle;

impl Syscall for SysPivotRootHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let new_root_ptr = Self::new_root(args);
        let put_old_ptr = Self::put_old(args);

        if new_root_ptr.is_null() || put_old_ptr.is_null() {
            return Err(SystemError::EFAULT);
        }

        // 解析路径
        let new_root_path = vfs_check_and_clone_cstr(new_root_ptr, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let put_old_path = vfs_check_and_clone_cstr(put_old_ptr, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        let new_root_path = new_root_path.trim();
        let put_old_path = put_old_path.trim();

        // log::info!(
        //     "[pivot_root] called with new_root='{}', put_old='{}'",
        //     new_root_path,
        //     put_old_path
        // );

        if new_root_path.is_empty() || put_old_path.is_empty() {
            log::error!("[pivot_root] empty path");
            return Err(SystemError::ENOENT);
        }

        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        // 权限检查：需要 CAP_SYS_CHROOT
        if !cred.has_capability(CAPFlags::CAP_SYS_CHROOT) {
            log::error!("[pivot_root] permission denied: no CAP_SYS_CHROOT");
            return Err(SystemError::EPERM);
        }

        // 解析 new_root 路径
        let (new_root_inode_begin, new_root_resolved) = user_path_at(
            &pcb,
            crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
            new_root_path,
        )?;
        let new_root_inode = new_root_inode_begin
            .lookup_follow_symlink(&new_root_resolved, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

        // log::info!(
        //     "[pivot_root] new_root_inode type: {:?}, fs: {}",
        //     new_root_inode.type_id(),
        //     new_root_inode.fs().name()
        // );

        // 验证 new_root 是目录
        let new_root_meta = new_root_inode.metadata()?;
        if new_root_meta.file_type != FileType::Dir {
            log::error!(
                "[pivot_root] new_root is not a directory: {:?}",
                new_root_meta.file_type
            );
            return Err(SystemError::ENOTDIR);
        }

        // 目录搜索权限
        cred.inode_permission(&new_root_meta, PermissionMask::MAY_EXEC.bits())?;

        // 获取 new_root 对应的 MountFS
        let new_root_mntfs = match Self::get_mountfs(&new_root_inode) {
            Ok(mntfs) => {
                log::info!("[pivot_root] new_root_mntfs: id={:?}", mntfs.mount_id());
                mntfs
            }
            Err(e) => {
                log::error!("[pivot_root] failed to get new_root MountFS: {:?}", e);
                return Err(e);
            }
        };

        // 解析 put_old 路径（相对于 new_root）
        let (put_old_inode_begin, put_old_resolved) = user_path_at(
            &pcb,
            crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
            put_old_path,
        )?;
        let put_old_inode = put_old_inode_begin
            .lookup_follow_symlink(&put_old_resolved, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

        // log::info!(
        //     "[pivot_root] put_old_inode type: {:?}, fs: {}",
        //     put_old_inode.type_id(),
        //     put_old_inode.fs().name()
        // );

        // 验证 put_old 是目录
        let put_old_meta = put_old_inode.metadata()?;
        if put_old_meta.file_type != FileType::Dir {
            log::error!(
                "[pivot_root] put_old is not a directory: {:?}",
                put_old_meta.file_type
            );
            return Err(SystemError::ENOTDIR);
        }

        // 目录搜索权限
        cred.inode_permission(&put_old_meta, PermissionMask::MAY_EXEC.bits())?;

        // 获取 put_old 对应的 MountFS
        let put_old_mntfs = match Self::get_mountfs(&put_old_inode) {
            Ok(mntfs) => {
                log::info!("[pivot_root] put_old_mntfs: id={:?}", mntfs.mount_id());
                mntfs
            }
            Err(e) => {
                log::error!("[pivot_root] failed to get put_old MountFS: {:?}", e);
                return Err(e);
            }
        };

        // 验证 new_root 和 put_old 在不同的挂载点
        // 注意：如果 new_root 和 put_old 是同一个 inode（如 "." 和 "."），Linux 会特殊处理
        let is_same_inode = new_root_meta.inode_id == put_old_meta.inode_id
            && Arc::ptr_eq(&new_root_inode.fs(), &put_old_inode.fs());

        if is_same_inode {
            // log::info!("[pivot_root] new_root and put_old are the same inode, special handling");

            // Linux 的 pivot_root 允许 new_root 和 put_old 相同
            // 在这种情况下，我们需要：
            // 1. 将 new_root 设置为新的根
            // 2. 旧根会自动被隐藏（不创建 put_old 挂载点）
            // 简化实现：我们只需要改变根挂载点
        } else if Arc::ptr_eq(&new_root_mntfs, &put_old_mntfs) {
            log::error!("[pivot_root] new_root and put_old are on the same mount point but different inodes");
            return Err(SystemError::EINVAL);
        } else {
            // 验证 put_old 是 new_root 的后代（即 put_old 在 new_root 之下）
            let is_descendant = is_ancestor_limited(&new_root_inode, &put_old_inode)?;
            if !is_descendant {
                log::error!("[pivot_root] put_old is not a descendant of new_root");
                return Err(SystemError::EINVAL);
            }
        }

        // 验证当前工作目录不在 put_old 中
        // 只有当 new_root 和 put_old 不同时才需要检查
        if !is_same_inode {
            let cwd = pcb.fs_struct().pwd();
            let cwd_in_putold = is_ancestor_limited(&put_old_inode, &cwd)?;
            if cwd_in_putold {
                log::error!("[pivot_root] current working directory is inside put_old");
                return Err(SystemError::EBUSY);
            }
        }

        // 执行 pivot_root
        // 获取当前进程的挂载命名空间
        let mnt_ns = ProcessManager::current_mntns();

        // 1. 保存旧的根 MountFS（用于后续挂到 put_old）
        let old_root_mntfs = mnt_ns.root_mntfs().clone();
        let old_root_inode = mnt_ns.root_inode();
        let post_pivot_put_old_path = if !is_same_inode {
            Some(Self::compute_post_pivot_put_old_path(
                &new_root_inode,
                &put_old_inode,
            )?)
        } else {
            None
        };

        // 1.1 将旧根挂到 new_root 下的 put_old（Linux pivot_root 核心语义）
        if !is_same_inode {
            let put_old_mountpoint = Self::make_mountpoint_inode(&put_old_inode, &new_root_mntfs);
            new_root_mntfs.add_mount(put_old_meta.inode_id, old_root_mntfs.clone())?;
            old_root_mntfs.set_self_mountpoint(put_old_mountpoint);
        }

        // log::info!(
        //     "[pivot_root] changing root mountfs from {:?} to {:?}",
        //     old_root_mntfs.mount_id(),
        //     new_root_mntfs.mount_id()
        // );

        // 2. 更新挂载命名空间的根
        // 使用 force_change_root_mountfs 来改变根挂载点
        // 注意：这是一个简化的实现，实际 Linux 的 pivot_root 更复杂
        unsafe {
            mnt_ns.force_change_root_mountfs(new_root_mntfs.clone());
        }

        // 3. 将旧根登记到 put_old 路径，便于按路径可见和后续 umount2(put_old)
        if let Some(put_old_mount_path) = post_pivot_put_old_path {
            mnt_ns.add_mount(
                Some(put_old_meta.inode_id),
                Arc::new(MountPath::from(put_old_mount_path.as_str())),
                old_root_mntfs,
            )?;
        }

        // 4. 更新同一挂载命名空间内所有任务的根目录和当前目录
        Self::reset_tasks_fs_for_namespace(&mnt_ns, &new_root_inode);

        // 5. 兼容 new_root == put_old 特例：调用任务 cwd 指向旧根，便于 umount2(".")
        //
        // 这一步必须放在批量同步之后，否则会被覆盖掉。
        if is_same_inode {
            pcb.fs_struct_mut().set_pwd(old_root_inode);
        }

        // 6. 更新进程的根目录
        // 关键修复：使用 new_root_inode 而不是 new_root_mntfs.root_inode()
        //
        // 原因：DragonOS 的 bind mount 实现与 Linux 有差异。
        // 在 Linux 中，bind mount 创建的挂载点以源目录为根，但在 DragonOS 中，
        // MountFS 包装的是整个底层文件系统，所以 MountFS::root_inode() 返回的是
        // 底层文件系统的根目录，而不是 bind source 目录的内容。
        //
        // 因此，我们需要直接使用 new_root_inode（bind source 目录的 inode）作为新的根目录。
        // 这样容器启动时能看到 rootfs 的内容（bin, lib 等），而不是底层文件系统的根目录。
        pcb.fs_struct_mut().set_root(new_root_inode.clone());

        // log::info!(
        //     "[pivot_root] SUCCESS: new_root='{}', put_old='{}', new_root_mntfs={:?}",
        //     new_root_path,
        //     put_old_path,
        //     new_root_mntfs.mount_id()
        // );

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

    /// 获取 inode 对应的 MountFS
    ///
    /// 如果 inode 是 MountFSInode，直接返回其所在的 MountFS。
    /// 否则，通过挂载命名空间查找匹配的 MountFS。
    ///
    /// 查找策略（按优先级排序）：
    /// 1. 直接从 MountFSInode 获取
    /// 2. 查找设置了 bind_target_root 的 MountFS（bind mount 标记）
    /// 3. 通过文件系统指针查找
    fn get_mountfs(inode: &Arc<dyn IndexNode>) -> Result<Arc<MountFS>, SystemError> {
        // 1. 尝试直接从 MountFSInode 获取
        if let Some(mountfs_inode) = inode.as_any_ref().downcast_ref::<MountFSInode>() {
            return Ok(mountfs_inode.mount_fs().clone());
        }

        let mnt_ns = ProcessManager::current_mntns();
        let inode_fs = inode.fs();

        // 2. 查找 bind mount 候选
        if let Some(mount_fs) = Self::find_bind_mount(&mnt_ns, &inode_fs) {
            return Ok(mount_fs);
        }

        // 3. 通过文件系统指针查找
        if let Some(mount_fs) = mnt_ns.mount_list().find_mount_by_fs(&inode_fs) {
            log::debug!("[pivot_root] get_mountfs: found MountFS by filesystem lookup");
            return Ok(mount_fs);
        }

        log::error!(
            "[pivot_root] get_mountfs: cannot find MountFS for inode type={:?}, fs={}",
            inode.type_id(),
            inode_fs.name()
        );
        Err(SystemError::EINVAL)
    }

    /// 查找 bind mount 创建的 MountFS
    ///
    /// 在挂载列表中查找包装了指定文件系统且设置了 bind_target_root 的 MountFS。
    /// 如果有多个候选，选择路径最短的（最上层的）。
    fn find_bind_mount(
        mnt_ns: &Arc<MntNamespace>,
        inode_fs: &Arc<dyn crate::filesystem::vfs::FileSystem>,
    ) -> Option<Arc<MountFS>> {
        let mount_list = mnt_ns.mount_list().clone_inner();
        let mut bind_mount_candidates: Vec<(Arc<MountFS>, usize)> = Vec::new();

        for (path, mnt_fs) in mount_list.iter() {
            let inner_fs = mnt_fs.inner_filesystem();
            // 按文件系统对象身份匹配，避免同类型多挂载时误选
            if Arc::ptr_eq(&inner_fs, inode_fs) && mnt_fs.bind_target_root().is_some() {
                let path_len = path.as_str().len();
                bind_mount_candidates.push((mnt_fs.clone(), path_len));
                log::debug!(
                    "[pivot_root] find_bind_mount: candidate id={:?}, path={:?}",
                    mnt_fs.mount_id(),
                    path.as_str()
                );
            }
        }

        // 选择路径最短的（最上层的）
        if !bind_mount_candidates.is_empty() {
            bind_mount_candidates.sort_by_key(|a| a.1);
            let (mnt_fs, _) = bind_mount_candidates.into_iter().next()?;
            log::debug!(
                "[pivot_root] find_bind_mount: returning id={:?}",
                mnt_fs.mount_id()
            );
            return Some(mnt_fs);
        }

        None
    }

    fn make_mountpoint_inode(
        put_old_inode: &Arc<dyn IndexNode>,
        new_root_mntfs: &Arc<MountFS>,
    ) -> Arc<MountFSInode> {
        if let Some(mountfs_inode) = put_old_inode.as_any_ref().downcast_ref::<MountFSInode>() {
            return mountfs_inode.clone_with_new_mount_fs(new_root_mntfs.clone());
        }
        MountFSInode::new(put_old_inode.clone(), new_root_mntfs.clone())
    }

    fn compute_post_pivot_put_old_path(
        new_root_inode: &Arc<dyn IndexNode>,
        put_old_inode: &Arc<dyn IndexNode>,
    ) -> Result<String, SystemError> {
        let new_root_abs = new_root_inode.absolute_path()?;
        let put_old_abs = put_old_inode.absolute_path()?;
        if new_root_abs == "/" {
            return Ok(put_old_abs);
        }
        let remain = put_old_abs
            .strip_prefix(new_root_abs.as_str())
            .ok_or(SystemError::EINVAL)?;

        if !remain.is_empty() && !remain.starts_with('/') {
            return Err(SystemError::EINVAL);
        }

        if remain.is_empty() {
            return Ok(String::from("/"));
        }

        Ok(String::from(remain))
    }

    fn reset_tasks_fs_for_namespace(mnt_ns: &Arc<MntNamespace>, new_root: &Arc<dyn IndexNode>) {
        for pid in ProcessManager::get_all_processes() {
            if let Some(task) = ProcessManager::find(pid) {
                let task_mnt_ns = task.nsproxy().mnt_namespace().clone();
                if Arc::ptr_eq(&task_mnt_ns, mnt_ns) {
                    let fs = task.fs_struct();
                    fs.set_root(new_root.clone());
                    fs.set_pwd(new_root.clone());
                }
            }
        }
    }
}

syscall_table_macros::declare_syscall!(SYS_PIVOT_ROOT, SysPivotRootHandle);
