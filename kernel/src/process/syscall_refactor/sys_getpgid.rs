use crate::arch::syscall::nr::SYS_GETPGID;
use crate::process::Pid;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGetPgid;

impl SysGetPgid {
    fn pid(args: &[usize]) -> Pid {
        Pid::new(args[0])
    }
}

impl Syscall for SysGetPgid {
    fn num_args(&self) -> usize {
        1
    }

    /// # 函数的功能
    /// 获取指定进程的pgid
    ///
    /// ## 参数
    /// - pid: 指定一个进程号
    ///
    /// ## 返回值
    /// - 成功，指定进程的进程组id
    /// - 错误，不存在该进程
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        if pid == Pid(0) {
            let current_pcb = ProcessManager::current_pcb();
            return Ok(current_pcb.pgid().into());
        }
        let target_proc = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        return Ok(target_proc.pgid().into());
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "pid",
            format!("{:#x}", Self::pid(args).0),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPGID, SysGetPgid);
