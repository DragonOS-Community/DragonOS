use crate::arch::syscall::nr::SYS_PRLIMIT64;
use crate::process::resource::RLimit64;
use crate::process::Pid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

use super::sys_getrlimit::do_prlimit64;

pub struct SysPrlimit64;

impl SysPrlimit64 {
    fn pid(args: &[usize]) -> Pid {
        Pid::new(args[0])
    }

    fn resource(args: &[usize]) -> usize {
        args[1]
    }

    fn new_limit(args: &[usize]) -> *const RLimit64 {
        args[2] as *const RLimit64
    }

    fn old_limit(args: &[usize]) -> *mut RLimit64 {
        args[3] as *mut RLimit64
    }
}

impl Syscall for SysPrlimit64 {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let resource = Self::resource(args);
        let new_limit = Self::new_limit(args);
        let old_limit = Self::old_limit(args);

        do_prlimit64(pid, resource, new_limit, old_limit)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args).data())),
            FormattedSyscallParam::new("resource", format!("{:#x}", Self::resource(args))),
            FormattedSyscallParam::new(
                "new_limit",
                format!("{:#x}", Self::new_limit(args) as usize),
            ),
            FormattedSyscallParam::new(
                "old_limit",
                format!("{:#x}", Self::old_limit(args) as usize),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PRLIMIT64, SysPrlimit64);
