use crate::arch::syscall::nr::SYS_SETGID;
use crate::process::ProcessManager;
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

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let gid = Self::gid(args);
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        if guard.egid.data() == 0 {
            guard.setgid(gid);
            guard.setegid(gid);
            guard.setsgid(gid);
            guard.setfsgid(gid);
        } else if guard.gid.data() == gid || guard.sgid.data() == gid {
            guard.setegid(gid);
            guard.setfsgid(gid);
        } else {
            return Err(SystemError::EPERM);
        }

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
