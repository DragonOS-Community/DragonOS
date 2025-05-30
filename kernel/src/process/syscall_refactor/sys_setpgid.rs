use crate::arch::syscall::nr::SYS_SETPGID;
use crate::process::Pgid;
use crate::process::Pid;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetPgid;

impl SysSetPgid {
    fn pid(args: &[usize]) -> Pid {
        Pid::new(args[0])
    }

    fn pgid(args: &[usize]) -> Pgid {
        Pgid::new(args[1])
    }
}

impl Syscall for SysSetPgid {
    fn num_args(&self) -> usize {
        2
    }

    /// # 函数的功能
    /// 设置指定进程的pgid
    ///
    /// ## 参数
    /// - pid: 指定进程号
    /// - pgid: 新的进程组号
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let pgid = Self::pgid(args);

        let current_pcb = ProcessManager::current_pcb();
        let pid = if pid == Pid(0) {
            current_pcb.pid()
        } else {
            pid
        };
        let pgid = if pgid == Pgid::from(0) {
            Pgid::from(pid.into())
        } else {
            pgid
        };
        if pid != current_pcb.pid() && !current_pcb.contain_child(&pid) {
            return Err(SystemError::ESRCH);
        }

        if pgid.into() != pid.into() && ProcessManager::find_process_group(pgid).is_none() {
            return Err(SystemError::EPERM);
        }
        let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        pcb.join_other_group(pgid)?;

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args).0)),
            FormattedSyscallParam::new("pgid", format!("{:#x}", Self::pgid(args).0)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETPGID, SysSetPgid);
