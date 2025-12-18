use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FCHMOD;
use crate::filesystem::vfs::file::FileFlags;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{open::do_fchmod, InodeMode},
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;

pub struct SysFchmodHandle;

impl Syscall for SysFchmodHandle {
    fn num_args(&self) -> usize {
        2
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let mode = Self::mode(args);

        let mode = InodeMode::from_bits(mode).ok_or(SystemError::EINVAL)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // Linux 语义：对 O_PATH fd 执行 fchmod 应返回 EBADF
        if file.flags().contains(FileFlags::O_PATH) {
            return Err(SystemError::EBADF);
        }

        // 通过 inode 修改元数据（保留文件类型位，仅替换权限/特殊位）
        // 注意：read()/write() 权限只在 open 时检查，chmod 不影响已打开 fd 的读写能力。
        do_fchmod(file.inode(), mode)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{:#x}", Self::fd(args))),
            FormattedSyscallParam::new("mode", format!("{:#x}", Self::mode(args))),
        ]
    }
}

impl SysFchmodHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn mode(args: &[usize]) -> u32 {
        args[1] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_FCHMOD, SysFchmodHandle);
