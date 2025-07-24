use core::ffi::c_int;

use crate::arch::syscall::nr::SYS_DUP;
use crate::{
    arch::interrupt::TrapFrame,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

/// @brief 根据提供的文件描述符的fd，复制对应的文件结构体，并返回新复制的文件结构体对应的fd
pub struct SysDupHandle;

impl Syscall for SysDupHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let oldfd = Self::oldfd(args);
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let old_file = fd_table_guard
            .get_file_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;

        let new_file = old_file.try_clone().ok_or(SystemError::EBADF)?;
        // dup默认非cloexec
        new_file.set_close_on_exec(false);
        // 申请文件描述符，并把文件对象存入其中
        let res = fd_table_guard.alloc_fd(new_file, None).map(|x| x as usize);
        return res;
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "oldfd",
            format!("{:#x}", Self::oldfd(args)),
        )]
    }
}

impl SysDupHandle {
    fn oldfd(args: &[usize]) -> c_int {
        args[0] as c_int
    }
}

syscall_table_macros::declare_syscall!(SYS_DUP, SysDupHandle);
