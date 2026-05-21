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
        let old_cred = pcb.cred();

        let old_fsuid = old_cred.fsuid;

        if fsuid == old_cred.uid || fsuid == old_cred.euid || fsuid == old_cred.suid {
            let mut new_cred: Cred = (*old_cred).clone();
            new_cred.setfsuid(fsuid.data());
            pcb.set_cred(Cred::new_arc(new_cred))?;
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
