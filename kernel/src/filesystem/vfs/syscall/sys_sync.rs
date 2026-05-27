use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_SYNC, SYS_SYNCFS},
    },
    filesystem::vfs::{file::FileFlags, FileSystem},
    libs::casting::DowncastArc,
    mm::page::PageReclaimer,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

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
        // 唤醒回写线程，异步刷回所有脏页
        PageReclaimer::flush_dirty_pages();

        let mounts = ProcessManager::current_mntns().mount_list().clone_inner();

        // 逐 superblock 同步回写脏 inode
        for (_path, mountfs) in &mounts {
            if !mountfs.is_readonly() {
                let _ = mountfs.sync_inodes_of_mount();
            }
        }

        // 逐 superblock 调 sync_fs(nowait)，提交元数据但不等待
        for (_path, mountfs) in &mounts {
            if !mountfs.is_readonly() {
                let _ = mountfs.sync_fs(false);
            }
        }

        // 逐 superblock 调 sync_fs(wait)，等待元数据落盘
        for (_path, mountfs) in &mounts {
            if !mountfs.is_readonly() {
                let _ = mountfs.sync_fs(true);
            }
        }

        // TODO: sync_bdevs(false) + sync_bdevs(true)

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

        // fdget(fd): EBADF if invalid fd
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        drop(fd_table_guard);

        // fdget() 通过 FMODE_PATH 掩码过滤 O_PATH fd
        if file.flags().contains(FileFlags::O_PATH) {
            return Err(SystemError::EBADF);
        }

        let inode = file.inode();
        // 非 VFS fd（pipe/socket 等）：其 sb 为只读伪文件系统（pipefs/sockfs），
        let mount_inode = match inode
            .clone()
            .downcast_arc::<crate::filesystem::vfs::mount::MountFSInode>()
        {
            Some(mi) => mi,
            None => return Ok(0),
        };

        let mount_fs = mount_inode.mount_fs();

        // TODO: down_read(&sb->s_umount) — 防止 sync 期间 umount

        // sync_filesystem(sb)
        let ret = mount_fs.sync_filesystem();

        // TODO: up_read(&sb->s_umount)

        // TODO: errseq_check_and_advance(&sb->s_wb_err, &f.file->f_sb_err)
        //       Linux syncfs 检查 per-sb 的 s_wb_err（异步写回错误）。
        //       DragonOS 已有 per-page-cache 的 errseq（PageCache::writeback_error + File::wb_error_seq），
        //       但缺少 per-sb 的聚合 errseq，后续可在 FileSystem trait 中添加。

        ret.map(|_| 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", format!("{}", args[0]))]
    }
}

syscall_table_macros::declare_syscall!(SYS_SYNCFS, SysSyncFsHandle);
