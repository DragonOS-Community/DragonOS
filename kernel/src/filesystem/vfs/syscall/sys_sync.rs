use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_SYNC, SYS_SYNCFS},
    },
    filesystem::{
        page_cache::list_page_caches,
        vfs::{file::FileFlags, mount::MountFS, FileSystem},
    },
    libs::casting::DowncastArc,
    mm::page::PageReclaimer,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, sync::Arc, vec::Vec};
use system_error::SystemError;

/// 回写指定 mount 下所有脏 page cache 的页数据。
///
/// DragonOS 没有 per-superblock 脏 inode 列表，
/// 因此通过全局 `PAGECACHE_REGISTRY` 遍历所有 page cache，
/// 比较 inode 所属的 `FileSystem` 与目标 mount 的内部文件系统指针来判断归属，
/// 若匹配则调用 `page_cache.manager().sync()` 回写脏页。
fn sync_inodes_of_mount(target_mount: &Arc<MountFS>) -> Result<(), SystemError> {
    let inner_fs = target_mount.inner_filesystem();
    let caches = list_page_caches();
    let mut last_err = Ok(());
    for page_cache in caches {
        let belongs = page_cache
            .inode()
            .and_then(|weak| weak.upgrade())
            .is_some_and(|inode| Arc::ptr_eq(&inode.fs(), &inner_fs));

        if belongs {
            if let Err(e) = page_cache.manager().sync() {
                log::warn!("sync_inodes_of_mount: page cache sync failed: {:?}", e);
                last_err = Err(e);
            }
        }
    }
    last_err
}

/// sync() 将所有挂起的文件系统元数据和缓存文件数据写入底层文件系统。
pub struct SysSyncHandle;

impl Syscall for SysSyncHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(
        &self,
        _args: &[usize],
        _frame: &mut TrapFrame,
    ) -> Result<usize, system_error::SystemError> {
        // 阶段1: 唤醒回写 + 回写所有脏页
        PageReclaimer::flush_dirty_pages();

        let mounts = ProcessManager::current_mntns().mount_list().clone_inner();

        // 阶段2: sync_inodes + sync_fs(nowait)
        for (_path, mountfs) in &mounts {
            if !mountfs.is_readonly() {
                let _ = sync_inodes_of_mount(&mountfs);
            }
        }

        for (_path, mountfs) in &mounts {
            if !mountfs.is_readonly() {
                let _ = mountfs.sync_fs(false);
            }
        }

        // 阶段3: sync_fs(wait)
        for (_path, mountfs) in &mounts {
            if !mountfs.is_readonly() {
                let _ = mountfs.sync_fs(true);
            }
        }

        Ok(0)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "No arguments",
            "sync()".to_string(),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SYNC, SysSyncHandle);

/// syncfs() 与 sync() 类似，但只同步文件描述符 fd 所在的文件系统。
pub struct SysSyncFsHandle;

impl Syscall for SysSyncFsHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut TrapFrame,
    ) -> Result<usize, system_error::SystemError> {
        let fd = args[0] as i32;

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        drop(fd_table_guard);

        // fdget() 使用 FMODE_PATH 掩码过滤 O_PATH fd
        if file.flags().contains(FileFlags::O_PATH) {
            return Err(SystemError::EBADF);
        }

        let inode = file.inode();
        // DragonOS: file.inode() 对于 VFS 文件返回 MountFSInode
        // 对于 pipe/socket 等非 VFS fd，无 MountFSInode 包装，
        // 对齐 Linux 行为：这些 fd 的 sb 是只读伪文件系统（如 pipefs），sync_filesystem 直接返回 0
        let mount_inode = match inode
            .clone()
            .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
        {
            Some(mi) => mi,
            None => return Ok(0),
        };

        let mount_fs = mount_inode.mount_fs();

        // 只读直接返回
        if mount_fs.is_readonly() {
            return Ok(0);
        }

        // writeback_inodes_sb(sb)
        sync_inodes_of_mount(&mount_fs)?;
        // sync_fs(sb, 0)
        mount_fs.sync_fs(false)?;
        // sync_inodes_sb(sb)
        sync_inodes_of_mount(&mount_fs)?;
        // sync_fs(sb, 1)
        mount_fs.sync_fs(true)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", format!("{}", args[0]))]
    }
}

syscall_table_macros::declare_syscall!(SYS_SYNCFS, SysSyncFsHandle);
