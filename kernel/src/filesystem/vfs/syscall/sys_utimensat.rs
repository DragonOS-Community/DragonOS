use system_error::SystemError;

use crate::{arch::interrupt::TrapFrame, syscall::table::{FormattedSyscallParam, Syscall}, time::PosixTimeSpec};
use alloc::vec::Vec;
use crate::arch::syscall::nr::SYS_UTIMENSAT;
pub struct SysUtimensatHandle;

impl Syscall for SysUtimensatHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let pathname = Self::pathname(args);
        let times = Self::times(args);
        let flags = Self::flags(args);
        super::utimensat::do_sys_utimensat(dirfd, pathname, times, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", Self::dirfd(args))),
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("times", format!("{:#x}", Self::times(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysUtimensatHandle {
    fn dirfd(args:&[usize])->i32{
        args[0] as i32
    }

    fn pathname(args:&[usize])->*const u8{
        args[1] as *const u8
    }

    fn times(args:&[usize])->*const PosixTimeSpec{
        args[2] as *const PosixTimeSpec
    }

    fn flags(args:&[usize])->u32{
        args[3] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_UTIMENSAT, SysUtimensatHandle);