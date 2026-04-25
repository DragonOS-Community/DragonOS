use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_EVENTFD;
use crate::filesystem::vfs::syscall::sys_eventfd2::do_eventfd;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::vec::Vec;
use system_error::SystemError;

/// System call handler for the `eventfd` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// creating an eventfd file descriptor (legacy version without flags).
pub struct SysEventFdHandle;

impl SysEventFdHandle {
    /// Extracts the initial value from syscall arguments
    fn initval(args: &[usize]) -> u32 {
        args[0] as u32
    }
}

impl Syscall for SysEventFdHandle {
    /// Returns the number of arguments expected by the `eventfd` syscall
    fn num_args(&self) -> usize {
        1
    }

    /// Handles the `eventfd` system call (legacy version)
    ///
    /// Creates an eventfd file descriptor with the specified initial value.
    /// This is the legacy version that doesn't support flags (flags default to 0).
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Initial value (u32)
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let initval = Self::initval(args);
        do_eventfd(initval, 0)
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - The raw syscall arguments
    ///
    /// # Returns
    /// Vector of formatted parameters with descriptive names
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "initval",
            Self::initval(args).to_string(),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_EVENTFD, SysEventFdHandle);
