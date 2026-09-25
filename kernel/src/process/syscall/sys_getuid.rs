use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETUID;
use crate::process::namespace::user_namespace::from_kuid_munged;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGetUid;

impl Syscall for SysGetUid {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        Ok(from_kuid_munged(&cred.user_ns, cred.uid) as usize)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETUID, SysGetUid);
