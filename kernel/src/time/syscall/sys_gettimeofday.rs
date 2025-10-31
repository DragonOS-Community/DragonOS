use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETTIMEOFDAY;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use crate::time::syscall::{PosixTimeZone, PosixTimeval, SYS_TIMEZONE};
use crate::time::timekeeping::do_gettimeofday;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGettimeofday;

impl SysGettimeofday {
    fn timeval_ptr(args: &[usize]) -> *mut PosixTimeval {
        args[0] as *mut PosixTimeval
    }

    fn timezone_ptr(args: &[usize]) -> *mut PosixTimeZone {
        args[1] as *mut PosixTimeZone
    }
}

impl Syscall for SysGettimeofday {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tv = Self::timeval_ptr(args);
        let timezone = Self::timezone_ptr(args);

        // TODO; 处理时区信息
        if tv.is_null() {
            return Err(SystemError::EFAULT);
        }
        let mut tv_buf =
            UserBufferWriter::new::<PosixTimeval>(tv, core::mem::size_of::<PosixTimeval>(), true)?;

        let tz_buf = if timezone.is_null() {
            None
        } else {
            Some(UserBufferWriter::new::<PosixTimeZone>(
                timezone,
                core::mem::size_of::<PosixTimeZone>(),
                true,
            )?)
        };

        let posix_time = do_gettimeofday();

        tv_buf.copy_one_to_user(&posix_time, 0)?;

        if let Some(mut tz_buf) = tz_buf {
            tz_buf.copy_one_to_user(&SYS_TIMEZONE, 0)?;
        }

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new(
                "timeval",
                format!("{:#x}", Self::timeval_ptr(args) as usize),
            ),
            FormattedSyscallParam::new(
                "timezone",
                format!("{:#x}", Self::timezone_ptr(args) as usize),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETTIMEOFDAY, SysGettimeofday);
