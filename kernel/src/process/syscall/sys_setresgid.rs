use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETRESGID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysSetResGid;

impl SysSetResGid {
    fn egid(args: &[usize]) -> usize {
        args[1]
    }
}

impl Syscall for SysSetResGid {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let egid = Self::egid(args);
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        if egid == usize::MAX || (egid == guard.egid.data() && egid == guard.fsgid.data()) {
            return Ok(0);
        }

        if egid != usize::MAX {
            guard.setegid(egid);
        }

        let egid = guard.egid.data();
        guard.setfsgid(egid);

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "egid",
            format!("{:#x}", Self::egid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETRESGID, SysSetResGid);
