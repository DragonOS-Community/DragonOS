use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETFSUID;
use crate::process::cred::Cred;
use crate::process::cred::Kuid;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysSetFsuid;

impl SysSetFsuid {
    fn fsuid(args: &[usize]) -> usize {
        args[0]
    }
}

impl Syscall for SysSetFsuid {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fsuid = Self::fsuid(args);
        let fsuid = Kuid::new(fsuid);

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_fsuid = guard.fsuid;

        if fsuid == guard.uid || fsuid == guard.euid || fsuid == guard.suid {
            let mut new_cred: Cred = (**guard).clone();
            new_cred.setfsuid(fsuid.data());
            *guard = Cred::new_arc(new_cred);
        }

        Ok(old_fsuid.data())
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "fsuid",
            format!("{:#x}", Self::fsuid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETFSUID, SysSetFsuid);
