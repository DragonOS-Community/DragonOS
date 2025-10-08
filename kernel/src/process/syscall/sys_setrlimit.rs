use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETRLIMIT;
use crate::process::resource::RLimit64;
use crate::process::syscall::sys_prlimit64::do_prlimit64;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;

use alloc::vec::Vec;

pub struct SysSetRlimit;

impl SysSetRlimit {
    fn resource(args: &[usize]) -> usize {
        args[0]
    }

    fn rlimit(args: &[usize]) -> *const RLimit64 {
        args[1] as *const RLimit64
    }
}

impl Syscall for SysSetRlimit {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let resource = Self::resource(args);
        let rlimit = Self::rlimit(args);

        do_prlimit64(
            ProcessManager::current_pcb().raw_pid(),
            resource,
            rlimit,
            core::ptr::null_mut::<RLimit64>(),
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("resource", format!("{:#x}", Self::resource(args))),
            FormattedSyscallParam::new("rlimit", format!("{:#x}", Self::rlimit(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETRLIMIT, SysSetRlimit);
