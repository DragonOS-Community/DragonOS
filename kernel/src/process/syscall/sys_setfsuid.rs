use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETFSUID;
use crate::process::cred::CAPFlags;
use crate::process::cred::Cred;
use crate::process::namespace::user_namespace::{from_kuid_munged, make_kuid};
use crate::process::syscall::id_utils;
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
        let pcb = ProcessManager::current_pcb();
        let old_cred = pcb.cred();
        let old_fsuid = old_cred.fsuid;
        let old_visible = from_kuid_munged(&old_cred.user_ns, old_fsuid) as usize;
        let Ok(fsuid) = make_kuid(&old_cred.user_ns, Self::fsuid(args) as u32) else {
            return Ok(old_visible);
        };

        if fsuid != old_fsuid
            && (fsuid == old_cred.uid
                || fsuid == old_cred.euid
                || fsuid == old_cred.suid
                || old_cred.has_capability(CAPFlags::CAP_SETUID))
        {
            let mut new_cred: Cred = (*old_cred).clone();
            new_cred.setfsuid(fsuid.data());
            id_utils::handle_fsuid_capabilities(&mut new_cred, old_fsuid.data());
            pcb.commit_cred(Cred::new_arc(new_cred))?;
        }

        Ok(old_visible)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "fsuid",
            format!("{:#x}", Self::fsuid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETFSUID, SysSetFsuid);
