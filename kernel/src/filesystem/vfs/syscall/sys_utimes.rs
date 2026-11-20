use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_UTIMES;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::time::syscall::PosixTimeval;
use alloc::vec::Vec;

pub struct SysUtimesHandle;

impl Syscall for SysUtimesHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        super::sys_futimesat::do_sys_futimesat(
            AtFlags::AT_FDCWD.bits(),
            Self::pathname(args),
            Self::times(args),
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("times", format!("{:#x}", Self::times(args) as usize)),
        ]
    }
}

impl SysUtimesHandle {
    fn pathname(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn times(args: &[usize]) -> *const PosixTimeval {
        args[1] as *const PosixTimeval
    }
}

syscall_table_macros::declare_syscall!(SYS_UTIMES, SysUtimesHandle);
