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

    /// 获取当前进程的tpid
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let tgid = current_pcb.task_tgid_vnr().ok_or(SystemError::ESRCH)?;
        if current_pcb.task_pid_vnr().data() == 1 && tgid.data() != 1 {
            log::error!(
                "Fixing inconsistent Init PID/TGID: PID=1, TGID={}",
                tgid.data()
            );
            return Ok(1);
        }
        log::debug!("getpid: returning tgid {}", tgid);
        Ok(tgid.into())
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPID, SysGetPid);
