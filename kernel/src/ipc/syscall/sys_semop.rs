use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{arch::syscall::nr::SYS_SEMOP, syscall::table::Syscall};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

use super::sys_semtimedop::do_user_semtimedop;

pub struct SysSemopHandle;

/// # SYS_SEMOP syscall: atomically execute a group of semaphore operations indefinitely
///
/// Shares its implementation with SYS_SEMTIMEDOP with a NULL timeout.
///
/// ## Parameters
///
/// - `semid`: semaphore set ID
/// - `sops`: userspace `sembuf` array pointer
/// - `nsops`: number of operations
///
/// ## Return value
///
/// On success: 0.
/// On failure: error code
impl Syscall for SysSemopHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_user_semtimedop(
            args[0] as i32,
            args[1] as *const u8,
            args[2] as u32 as usize,
            None,
            frame.is_from_user(),
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("semid", format!("{}", args[0])),
            FormattedSyscallParam::new("sops", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("nsops", format!("{}", args[2])),
        ]
    }
}

declare_syscall!(SYS_SEMOP, SysSemopHandle);
