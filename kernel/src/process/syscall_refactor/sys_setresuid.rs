use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETRESUID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysSetResUid;

impl SysSetResUid {
    fn euid(args: &[usize]) -> usize {
        args[1]
    }
}

impl Syscall for SysSetResUid {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let euid = Self::euid(args);
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        if euid == usize::MAX || (euid == guard.euid.data() && euid == guard.fsuid.data()) {
            return Ok(0);
        }

        if euid != usize::MAX {
            guard.seteuid(euid);
        }

        let euid = guard.euid.data();
        guard.setfsuid(euid);

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "euid",
            format!("{:#x}", Self::euid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETRESUID, SysSetResUid);
