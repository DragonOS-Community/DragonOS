use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_SYNC, SYS_SYNCFS},
    },
    filesystem::vfs::{file::FileFlags, mount::list_unique_mounted_superblocks},
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

        let mounts = list_unique_mounted_superblocks();

        // 逐 superblock 同步回写脏 inode
        for mountfs in &mounts {
            let _ = mountfs.sync_inodes_with_umount_read();
        }

        // 逐 superblock 调 sync_fs(nowait)，提交元数据但不等待
        for mountfs in &mounts {
            let _ = mountfs.sync_fs_with_umount_read(false);
        }

        // 逐 superblock 调 sync_fs(wait)，等待元数据落盘
        for mountfs in &mounts {
            let _ = mountfs.sync_fs_with_umount_read(true);
        }

        for mountfs in &mounts {
            let _ = mountfs.sync_blockdev_with_umount_read(false);
        }

        for mountfs in &mounts {
            let _ = mountfs.sync_blockdev_with_umount_read(true);
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

        let sync_result = mount_fs.sync_filesystem();
        let errseq_result = file.check_and_advance_sb_wb_error(&mount_fs);

        match sync_result {
            Err(e) => Err(e),
            Ok(()) => errseq_result.map(|_| 0),
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", format!("{}", args[0]))]
    }
}

syscall_table_macros::declare_syscall!(SYS_SYNCFS, SysSyncFsHandle);
