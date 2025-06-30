use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETSID;
use crate::process::ProcessManager;
use crate::process::RawPid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysGetsid;

impl SysGetsid {
    fn pid(args: &[usize]) -> RawPid {
        RawPid::new(args[0])
    }
}

impl Syscall for SysGetsid {
    fn num_args(&self) -> usize {
        1
    }

    /// # 函数的功能
    /// 获取指定进程的会话id
    ///
    /// 若pid为0，则返回当前进程的会话id
    ///
    /// 若pid不为0，则返回指定进程的会话id
    ///
    /// ## 参数
    /// - pid: 指定一个进程号
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let session = ProcessManager::current_pcb().session().unwrap();
        let sid = session.sid().into();
        if pid == RawPid(0) {
            return Ok(sid);
        }
        let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        if !Arc::ptr_eq(&session, &pcb.session().unwrap()) {
            return Err(SystemError::EPERM);
        }
        return Ok(sid);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "pid",
            format!("{:#x}", Self::pid(args).0),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETSID, SysGetsid);
