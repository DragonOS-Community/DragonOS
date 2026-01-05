use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_EVENTFD2;
use crate::filesystem::eventfd::{EventFd, EventFdFlags, EventFdInode, EVENTFD_ID_ALLOCATOR};
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

/// System call handler for the `eventfd2` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// creating an eventfd file descriptor with flags support.
pub struct SysEventFd2Handle;

impl SysEventFd2Handle {
    /// Extracts the initial value from syscall arguments
    fn initval(args: &[usize]) -> u32 {
        args[0] as u32
    }

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> u32 {
        args[1] as u32
    }
}

impl Syscall for SysEventFd2Handle {
    /// Returns the number of arguments expected by the `eventfd2` syscall
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the `eventfd2` system call
    ///
    /// Creates an eventfd file descriptor with the specified initial value and flags.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Initial value (u32)
    ///   - args[1]: Flags (u32): EFD_SEMAPHORE, EFD_CLOEXEC, EFD_NONBLOCK
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let initval = Self::initval(args);
        let flags = Self::flags(args);
        do_eventfd(initval, flags)
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - The raw syscall arguments
    ///
    /// # Returns
    /// Vector of formatted parameters with descriptive names
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("initval", Self::initval(args).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_EVENTFD2, SysEventFd2Handle);

/// Internal implementation of the eventfd operation
///
/// # Arguments
/// * `init_val` - Initial value for the eventfd
/// * `flags` - Flags for the eventfd (EFD_SEMAPHORE, EFD_CLOEXEC, EFD_NONBLOCK)
///
/// # Returns
/// * `Ok(usize)` - File descriptor on success
/// * `Err(SystemError)` - Error code if operation fails
///
/// See: https://man7.org/linux/man-pages/man2/eventfd2.2.html
pub fn do_eventfd(init_val: u32, flags: u32) -> Result<usize, SystemError> {
    let flags = EventFdFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
    let id = EVENTFD_ID_ALLOCATOR
        .lock()
        .alloc()
        .ok_or(SystemError::ENOMEM)? as u32;
    let eventfd = EventFd::new(init_val as u64, flags, id);
    let inode = Arc::new(EventFdInode::new(eventfd));
    let filemode = if flags.contains(EventFdFlags::EFD_CLOEXEC) {
        FileFlags::O_RDWR | FileFlags::O_CLOEXEC
    } else {
        FileFlags::O_RDWR
    };
    let file = File::new(inode, filemode)?;
    let binding = ProcessManager::current_pcb().fd_table();
    let mut fd_table_guard = binding.write();
    let fd = fd_table_guard.alloc_fd(file, None).map(|x| x as usize);
    return fd;
}
