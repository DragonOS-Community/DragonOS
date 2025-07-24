use core::ffi::c_int;

use crate::arch::syscall::nr::SYS_DUP2;
use crate::filesystem::vfs::syscall::dup2::do_dup2;
use crate::{
    arch::interrupt::TrapFrame,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

/// 根据提供的文件描述符的fd，和指定新fd，复制对应的文件结构体，
/// 并返回新复制的文件结构体对应的fd.
/// 如果新fd已经打开，则会先关闭新fd.
///
/// ## 参数
///
/// - `oldfd`：旧文件描述符
/// - `newfd`：新文件描述符
///
/// ## 返回值
///
/// - 成功：新文件描述符
/// - 失败：错误码
pub struct SysDup2Handle;

impl Syscall for SysDup2Handle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let oldfd = Self::oldfd(args);
        let newfd = Self::newfd(args);
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        return do_dup2(oldfd, newfd, &mut fd_table_guard);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("oldfd",format!("{:#x}", Self::oldfd(args))),
            FormattedSyscallParam::new("newfd",format!("{:#x}", Self::newfd(args)))
        ]
    }
}

impl SysDup2Handle {
    fn oldfd(args: &[usize]) -> c_int {
        args[0] as c_int
    }

    fn newfd(args: &[usize]) -> c_int {
        args[1] as c_int
    }
}

syscall_table_macros::declare_syscall!(SYS_DUP2, SysDup2Handle);
