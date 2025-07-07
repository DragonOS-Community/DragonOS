use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETPGID;
use crate::process::pid::Pid;
use crate::process::ProcessManager;
use crate::process::RawPid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGetPgid;

impl SysGetPgid {
    fn pid(args: &[usize]) -> RawPid {
        RawPid::new(args[0])
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
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        return do_getpgid(pid);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "pid",
            format!("{:#x}", Self::pid(args).0),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPGID, SysGetPgid);

/// 获取进程组ID
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sys.c#1144
pub(super) fn do_getpgid(pid: RawPid) -> Result<usize, SystemError> {
    let grp: Arc<Pid>;
    if pid == RawPid(0) {
        let current_pcb = ProcessManager::current_pcb();
        grp = current_pcb.task_pgrp().ok_or(SystemError::ESRCH)?;
    } else {
        let p = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;
        grp = p.task_pgrp().ok_or(SystemError::ESRCH)?;
    }

    let retval = grp.pid_vnr();
    Ok(retval.into())
}
