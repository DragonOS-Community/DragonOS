use crate::arch::syscall::nr::SYS_FTRUNCATE;
use crate::{
    arch::interrupt::TrapFrame,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

/// # ftruncate
///
/// ## 描述
///
/// 改变文件大小.
/// 如果文件大小大于原来的大小，那么文件的内容将会被扩展到指定的大小，新的空间将会用0填充.
/// 如果文件大小小于原来的大小，那么文件的内容将会被截断到指定的大小.
///
/// ## 参数
///
/// - `fd`：文件描述符
/// - `len`：文件大小
///
/// ## 返回值
///
/// 如果成功，返回0，否则返回错误码.
pub struct SysFtruncateHandle;

impl Syscall for SysFtruncateHandle {
    fn num_args(&self) -> usize {
        2
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let len = Self::len(args);

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
            // drop guard 以避免无法调度的问题
            drop(fd_table_guard);
            let r = file.ftruncate(len).map(|_| 0);
            return r;
        }

        return Err(SystemError::EBADF);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{:#x}", Self::fd(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
        ]
    }
}

impl SysFtruncateHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_FTRUNCATE,SysFtruncateHandle);
