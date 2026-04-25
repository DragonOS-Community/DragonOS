use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_TIME;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;

/// sys_time: time(time_t *tloc) -> time_t
///
/// Linux: returns seconds since the Epoch; if tloc != NULL, also stores it.
pub struct SysTimeHandle;

impl Syscall for SysTimeHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tloc = args[0] as *mut i64;

        // Use the same time source as gettimeofday.
        let now_ns = crate::time::timekeeping::do_gettimeofday().to_ns();
        let now_sec = (now_ns / 1_000_000_000) as i64;

        if !tloc.is_null() {
            let mut w = UserBufferWriter::new(
                tloc as *mut u8,
                core::mem::size_of::<i64>(),
                frame.is_from_user(),
            )?;
            w.buffer_protected(0)?.write_one::<i64>(0, &now_sec)?;
        }

        Ok(now_sec as usize)
    }

    fn entry_format(&self, args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        alloc::vec![FormattedSyscallParam::new(
            "tloc",
            format!("{:#x}", args[0]),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_TIME, SysTimeHandle);
