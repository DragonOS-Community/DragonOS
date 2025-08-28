use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETPID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysGetPid;

impl Syscall for SysGetPid {
    fn num_args(&self) -> usize {
        0
    }

    /// # 函数的功能
    /// 获取当前进程的pid
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();

        Ok(current_pcb.task_pid_vnr().into())
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPID, SysGetPid);
