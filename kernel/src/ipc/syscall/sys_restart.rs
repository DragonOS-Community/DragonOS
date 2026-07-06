use crate::arch::interrupt::TrapFrame;
use crate::{
    alloc::vec::Vec,
    arch::syscall::nr::SYS_RESTART_SYSCALL,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;
pub struct SysRestartHandle;

/// # SYS_RESTART_SYSCALL 系统调用函数，用于重启被信号中断的系统调用
///
/// ## 返回值
///
/// 根据被重启的系统调用决定
pub(super) fn do_kernel_restart_syscall() -> Result<usize, SystemError> {
    let restart_block = ProcessManager::current_pcb().restart_block().take();
    if let Some(mut restart_block) = restart_block {
        return restart_block.restart_fn.call(&mut restart_block.data);
    } else {
        // Align with Linux do_no_restart_syscall(): userspace can directly call
        // restart_syscall, so the absence of a restart block is a normal EINTR return,
        // not a kernel invariant violation that requires killing the process or panicking.
        return Err(SystemError::EINTR);
    }
}

impl Syscall for SysRestartHandle {
    fn num_args(&self) -> usize {
        0 // restart_syscall 通常没有参数
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new() // 没有参数，返回空Vec
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_kernel_restart_syscall()
    }
}

declare_syscall!(SYS_RESTART_SYSCALL, SysRestartHandle);
