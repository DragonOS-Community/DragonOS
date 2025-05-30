use system_error::SystemError;
use alloc::vec::Vec;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::arch::syscall::nr::SYS_GETPID;

pub struct SysGetPid;

impl Syscall for SysGetPid{
    fn num_args(&self) -> usize {
        0
    }

    /// # 函数的功能
    /// 获取当前进程的pid
    fn handle(&self, _args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        // if let Some(pid_ns) = &current_pcb.get_nsproxy().read().pid_namespace {
        //     // 获取该进程在命名空间中的 PID
        //     return Ok(current_pcb.pid_strcut().read().numbers[pid_ns.level].nr);
        //     // 返回命名空间中的 PID
        // }
        // 默认返回 tgid
        return Ok(current_pcb.tgid().into());
        
        
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    } 
}

syscall_table_macros::declare_syscall!(SYS_GETPID, SysGetPid);