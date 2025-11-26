use log::warn;
use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FCHMOD;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::syscall::InodeMode,
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

        let _mode = InodeMode::from_bits(mode).ok_or(SystemError::EINVAL)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let _file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // fchmod没完全实现，因此不修改文件的权限
        // todo: 实现fchmod
        warn!("fchmod not fully implemented");
        return Ok(0);
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
