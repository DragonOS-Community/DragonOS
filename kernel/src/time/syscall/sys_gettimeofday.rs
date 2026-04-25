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

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tv = Self::timeval_ptr(args);
        let timezone = Self::timezone_ptr(args);

        // TODO; 处理时区信息
        let posix_time = do_gettimeofday();

        // 如果 tv 不为空，使用 buffer_protected 来保护用户空间访问
        if !tv.is_null() {
            let mut tv_buf = UserBufferWriter::new::<PosixTimeval>(
                tv,
                core::mem::size_of::<PosixTimeval>(),
                frame.is_from_user(),
            )?;
            tv_buf.buffer_protected(0)?.write_one(0, &posix_time)?;
        }

        // 如果 timezone 不为空，使用 buffer_protected 来保护用户空间访问
        if !timezone.is_null() {
            let mut tz_buf = UserBufferWriter::new::<PosixTimeZone>(
                timezone,
                core::mem::size_of::<PosixTimeZone>(),
                frame.is_from_user(),
            )?;

            tz_buf.buffer_protected(0)?.write_one(0, &SYS_TIMEZONE)?;
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
