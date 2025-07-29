use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETUID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetUid;

impl SysSetUid {
    fn uid(args: &[usize]) -> usize {
        args[0]
    }
}

impl Syscall for SysSetUid {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let uid = Self::uid(args);
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_cred = guard.clone();
        let new_cred_mut = Arc::make_mut(&mut guard);

        if old_cred.uid.data() == 0 {
            new_cred_mut.setuid(uid);
            new_cred_mut.seteuid(uid);
            new_cred_mut.setsuid(uid);
        } else if uid == old_cred.uid.data() || uid == old_cred.suid.data() {
            new_cred_mut.seteuid(uid);
        } else {
            return Err(SystemError::EPERM);
        }

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "uid",
            format!("{:#x}", Self::uid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETUID, SysSetUid);
