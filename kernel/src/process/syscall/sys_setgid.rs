use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETGID;
use crate::process::ProcessManager;
use crate::process::cred::Cred;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetGid;

impl SysSetGid {
    fn gid(args: &[usize]) -> usize {
        args[0]
    }
}

impl Syscall for SysSetGid {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let gid = Self::gid(args);
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();
        let mut new_cred: Cred = (**guard).clone();

        if guard.egid.data() == 0 {
            new_cred.setgid(gid);
            new_cred.setegid(gid);
            new_cred.setsgid(gid);
            new_cred.setfsgid(gid);
        } else if guard.gid.data() == gid || guard.sgid.data() == gid {
            new_cred.setegid(gid);
            new_cred.setfsgid(gid);
        } else {
            return Err(SystemError::EPERM);
        }

        *guard = Cred::new_arc(new_cred);

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "gid",
            format!("{:#x}", Self::gid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETGID, SysSetGid);
