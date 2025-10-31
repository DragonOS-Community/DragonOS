use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    arch::syscall::nr::{SYS_GET_ROBUST_LIST, SYS_SET_ROBUST_LIST},
    mm::{verify_area, VirtAddr},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};

/// System call handler for the `set_robust_list` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for setting
/// the robust futex list for the current process.
pub struct SysSetRobustListHandle;

impl Syscall for SysSetRobustListHandle {
    /// Returns the number of arguments expected by the `set_robust_list` syscall
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the `set_robust_list` system call
    ///
    /// Sets the robust futex list for the current process, which is used for cleanup
    /// when the process exits while holding futexes.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: head - Pointer to the robust list head (*const PosixRobustListHead)
    ///   - args[1]: len - Length of the robust list head structure
    /// * `frame` - Trap frame containing execution context
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let head = Self::head(args);
        let len = Self::len(args);

        // 判断用户空间地址的合法性
        verify_area(head, core::mem::size_of::<u32>())?;

        crate::libs::futex::futex::RobustListHead::set_robust_list(head, len)
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("head", format!("{:#x}", Self::head(args).data())),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
        ]
    }
}

impl SysSetRobustListHandle {
    /// Extracts the robust list head pointer from syscall arguments
    fn head(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[0])
    }

    /// Extracts the length from syscall arguments
    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

/// System call handler for the `get_robust_list` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for getting
/// the robust futex list of a specified process.
pub struct SysGetRobustListHandle;

impl Syscall for SysGetRobustListHandle {
    /// Returns the number of arguments expected by the `get_robust_list` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `get_robust_list` system call
    ///
    /// Gets the robust futex list of a specified process.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: pid - Process ID (0 for current process)
    ///   - args[1]: head - Pointer to store the robust list head (*mut PosixRobustListHead)
    ///   - args[2]: len_ptr - Pointer to store the length (*mut usize)
    /// * `frame` - Trap frame containing execution context
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let head = Self::head(args);
        let len_ptr = Self::len_ptr(args);

        // 判断用户空间地址的合法性
        verify_area(head, core::mem::size_of::<u32>())?;
        verify_area(len_ptr, core::mem::size_of::<u32>())?;

        crate::libs::futex::futex::RobustListHead::get_robust_list(pid, head, len_ptr)
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("head", format!("{:#x}", Self::head(args).data())),
            FormattedSyscallParam::new("len_ptr", format!("{:#x}", Self::len_ptr(args).data())),
        ]
    }
}

impl SysGetRobustListHandle {
    /// Extracts the process ID from syscall arguments
    fn pid(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the robust list head pointer from syscall arguments
    fn head(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[1])
    }

    /// Extracts the length pointer from syscall arguments
    fn len_ptr(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[2])
    }
}

syscall_table_macros::declare_syscall!(SYS_SET_ROBUST_LIST, SysSetRobustListHandle);
syscall_table_macros::declare_syscall!(SYS_GET_ROBUST_LIST, SysGetRobustListHandle);
