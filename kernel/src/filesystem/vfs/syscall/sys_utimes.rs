use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_UTIMES;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::open::do_utimes;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use crate::syscall::user_access::check_and_clone_cstr;
use crate::time::syscall::PosixTimeval;
use alloc::vec::Vec;

pub struct SysUtimesHandle;

impl Syscall for SysUtimesHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pathname = Self::pathname(args);
        let times = Self::times(args);

        let pathname = check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let times = if times.is_null() {
            None
        } else {
            let times_reader = UserBufferReader::new(times, size_of::<PosixTimeval>() * 2, true)?;
            let times = times_reader.read_from_user::<PosixTimeval>(0)?;
            Some([times[0], times[1]])
        };
        do_utimes(&pathname, times)
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
