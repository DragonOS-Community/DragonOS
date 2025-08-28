use alloc::sync::Arc;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETFSGID;
use crate::process::cred::Cred;
use crate::process::cred::Kgid;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetFsgid;

impl SysSetFsgid {
    fn fsgid(args: &[usize]) -> usize {
        args[0]
    }
}

impl Syscall for SysSetFsgid {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fsgid = Self::fsgid(args);
        let fsgid = Kgid::new(fsgid);

        let pcb = ProcessManager::current_pcb();
        let mut guard: crate::libs::spinlock::SpinLockGuard<'_, Arc<Cred>> = pcb.cred.lock();
        let old_fsgid = guard.fsgid;

        if fsgid == guard.gid || fsgid == guard.egid || fsgid == guard.sgid {
            let mut new_cred: Cred = (**guard).clone();
            new_cred.setfsgid(fsgid.data());
            *guard = Cred::new_arc(new_cred);
        }

        Ok(old_fsgid.data())
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "fsgid",
            format!("{:#x}", Self::fsgid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETFSGID, SysSetFsgid);
