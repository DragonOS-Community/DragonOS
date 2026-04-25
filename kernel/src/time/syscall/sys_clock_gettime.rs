use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CLOCK_GETTIME;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use crate::time::{syscall::posix_clock_now, syscall::PosixClockID, PosixTimeSpec};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysClockGettime;

impl SysClockGettime {
    fn clock_id(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn timespec_ptr(args: &[usize]) -> *mut PosixTimeSpec {
        args[1] as *mut PosixTimeSpec
    }
}

impl Syscall for SysClockGettime {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let clock_id = PosixClockID::try_from(Self::clock_id(args))?;

        let tp = Self::timespec_ptr(args);
        if tp.is_null() {
            return Err(SystemError::EFAULT);
        }

        let mut tp_buf = UserBufferWriter::new::<PosixTimeSpec>(
            tp,
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;

        let timespec = posix_clock_now(clock_id);

        tp_buf.buffer_protected(0)?.write_one(0, &timespec)?;

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("clock_id", format!("{}", Self::clock_id(args))),
            FormattedSyscallParam::new(
                "timespec",
                format!("{:#x}", Self::timespec_ptr(args) as usize),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_CLOCK_GETTIME, SysClockGettime);
