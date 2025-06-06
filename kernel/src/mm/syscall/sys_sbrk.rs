//! System call handler for the sbrk system call.

use crate::arch::interrupt::TrapFrame;
use crate::mm::ucontext::AddressSpace;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::SYS_SBRK;
use system_error::SystemError;

use alloc::vec::Vec;

/// Handler for the sbrk system call, which increments the program's data space (heap).
pub struct SysSbrkHandle;

impl Syscall for SysSbrkHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        1
    }

    /// Handles the sbrk system call.
    ///
    /// # Arguments
    /// * `args` - The syscall arguments, where args[0] is the increment value (isize).
    ///
    /// # Returns
    /// * On success, returns the previous program break (heap end) as usize.
    /// * On failure, returns a SystemError.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let incr = Self::incr(args);
        let address_space = AddressSpace::current()?;
        assert!(address_space.read().user_mapper.utable.is_current());
        let mut address_space = address_space.write();
        let r = unsafe { address_space.sbrk(incr) }?;
        return Ok(r.data());
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "incr",
            format!("{}", Self::incr(args)),
        )]
    }
}

impl SysSbrkHandle {
    /// Extracts the increment argument from syscall parameters.
    fn incr(args: &[usize]) -> isize {
        args[0] as isize
    }
}

syscall_table_macros::declare_syscall!(SYS_SBRK, SysSbrkHandle);
