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
        let current = ProcessManager::current_pcb();
        let binding = current.fd_table();

        // dup 共享同一个 open file description（Arc<File>），cloexec 默认 false
        let res = binding
            .duplicate(oldfd, false, current.nofile_soft_limit())
            .map(|x| x as usize);
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
