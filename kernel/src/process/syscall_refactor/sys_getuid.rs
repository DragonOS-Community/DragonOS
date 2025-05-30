use crate::arch::syscall::nr::SYS_GETUID;
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

    fn handle(&self, _args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        return Ok(pcb.cred.lock().uid.data());
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETUID, SysGetUid);
