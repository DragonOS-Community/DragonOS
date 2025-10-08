use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_NANOSLEEP;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use crate::time::sleep::nanosleep;
use crate::time::PosixTimeSpec;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysNanosleep;

impl SysNanosleep {
    fn sleep_time(args: &[usize]) -> *const PosixTimeSpec {
        args[0] as *const PosixTimeSpec
    }

    fn rm_time(args: &[usize]) -> *mut PosixTimeSpec {
        args[1] as *mut PosixTimeSpec
    }
}

impl Syscall for SysNanosleep {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let sleep_time_reader = UserBufferReader::new(
            Self::sleep_time(args),
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;
        let rm_time_ptr = Self::rm_time(args);
        let mut rm_time_writer = if !rm_time_ptr.is_null() {
            Some(UserBufferWriter::new(
                rm_time_ptr,
                core::mem::size_of::<PosixTimeSpec>(),
                true,
            )?)
        } else {
            None
        };

        let sleep_time = sleep_time_reader.read_one_from_user::<PosixTimeSpec>(0)?;

        let slt_spec = PosixTimeSpec {
            tv_sec: sleep_time.tv_sec,
            tv_nsec: sleep_time.tv_nsec,
        };
        let r = nanosleep(slt_spec)?;
        if let Some(ref mut rm_time) = rm_time_writer {
            // 如果rm_time不为null，则将剩余时间写入rm_time
            rm_time.copy_one_to_user(&r, 0)?;
        }

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new(
                "sleep_time",
                format!("{:#x}", Self::sleep_time(args) as usize),
            ),
            FormattedSyscallParam::new("rm_time", format!("{:#x}", Self::rm_time(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_NANOSLEEP, SysNanosleep);
