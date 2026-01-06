use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_YIELD;
use crate::sched::sched_yield;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;

/// System call handler for the `sched_yield` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// voluntarily yielding the CPU to other processes.
struct SysSchedYield;

impl Syscall for SysSchedYield {
    /// Returns the number of arguments expected by the `sched_yield` syscall
    fn num_args(&self) -> usize {
        0
    }

    /// Handles the `sched_yield` system call
    ///
    /// Voluntarily yields the CPU to other processes. The current process
    /// will be moved to the end of the run queue for its priority level.
    ///
    /// # Arguments
    /// * `_args` - Array containing no arguments (unused)
    /// * `_frame` - Trap frame (unused in this implementation)
    ///
    /// # Returns
    /// * `Ok(0)`: Success
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        sched_yield();
        Ok(0)
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `_args` - The raw syscall arguments (unused, as sched_yield takes no arguments)
    ///
    /// # Returns
    /// Empty vector (sched_yield takes no arguments)
    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_YIELD, SysSchedYield);
