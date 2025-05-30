use super::super::signal_types::{SigInfo, SigType};
use crate::{
    alloc::vec::Vec,
    arch::ipc::signal::{SigCode, Signal},
    arch::syscall::nr::SYS_RESTART_SYSCALL,
    process::{Pid, ProcessManager},
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
        // 不应该走到这里，因此kill掉当前进程及同组的进程
        let pid = Pid::new(0);
        let sig = Signal::SIGKILL;
        let mut info = SigInfo::new(sig, 0, SigCode::Kernel, SigType::Kill(pid));

        sig.send_signal_info(Some(&mut info), pid)
            .expect("Failed to kill ");
        return Ok(0);
    }
}

impl Syscall for SysRestartHandle {
    fn num_args(&self) -> usize {
        0 // restart_syscall 通常没有参数
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new() // 没有参数，返回空Vec
    }

    fn handle(&self, _args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        do_kernel_restart_syscall()
    }
}

declare_syscall!(SYS_RESTART_SYSCALL, SysRestartHandle);
