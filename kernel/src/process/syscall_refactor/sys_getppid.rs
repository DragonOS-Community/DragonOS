use crate::arch::syscall::nr::SYS_GETPPID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGetPpid;

impl Syscall for SysGetPpid {
    fn num_args(&self) -> usize {
        0
    }

    /// # 函数的功能
    /// 获取当前进程的父进程id
    fn handle(&self, _args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.basic().ppid().into());
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPPID, SysGetPpid);
