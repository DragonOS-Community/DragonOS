use crate::arch::interrupt::TrapFrame;
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
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let parent_pcb = current_pcb.real_parent_pcb.read_irqsave().clone();
        let parent_pcb = parent_pcb.upgrade().ok_or(SystemError::ESRCH)?;

        let r = parent_pcb.task_tgid_vnr().ok_or(SystemError::ESRCH)?;
        return Ok(r.into());
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPPID, SysGetPpid);
