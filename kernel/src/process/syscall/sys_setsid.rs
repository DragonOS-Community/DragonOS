use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETSID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysSetsid;

impl Syscall for SysSetsid {
    fn num_args(&self) -> usize {
        0
    }

    /// # 函数的功能
    /// 创建新的会话
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let session = pcb.go_to_new_session()?;
        let mut guard = pcb.sig_info_mut();
        guard.set_tty(None);
        Ok(session.sid().into())
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETSID, SysSetsid);
