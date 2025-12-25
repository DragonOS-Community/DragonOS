use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETSOCKOPT;
use crate::mm::VirtAddr;
use crate::net::socket;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `setsockopt` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for setting socket options.
pub struct SysSetsockoptHandle;

impl Syscall for SysSetsockoptHandle {
    /// Returns the number of arguments expected by the `setsockopt` syscall
    fn num_args(&self) -> usize {
        5
    }

    /// Handles the `setsockopt` system call
    ///
    /// Sets a socket option.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Level (usize)
    ///   - args[2]: Option name (usize)
    ///   - args[3]: Option value pointer (*const u8)
    ///   - args[4]: Option value length (usize)
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let level = Self::level(args);
        let optname = Self::optname(args);
        let optval = Self::optval(args);
        let optlen = Self::optlen(args);

        // Verify optval address validity if from user space
        if frame.is_from_user() {
            let virt_optval = VirtAddr::new(optval as usize);
            if crate::mm::verify_area(virt_optval, optlen).is_err() {
                return Err(SystemError::EFAULT);
            }
        }

        // Read optval from user space
        let user_buffer_reader = UserBufferReader::new(optval, optlen, frame.is_from_user())?;
        let data = user_buffer_reader.read_from_user(0)?;

        do_setsockopt(fd, level, optname, data)
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
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("level", Self::level(args).to_string()),
            FormattedSyscallParam::new("optname", Self::optname(args).to_string()),
            FormattedSyscallParam::new("optval", format!("{:#x}", Self::optval(args) as usize)),
            FormattedSyscallParam::new("optlen", Self::optlen(args).to_string()),
        ]
    }
}

impl SysSetsockoptHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the level from syscall arguments
    fn level(args: &[usize]) -> usize {
        args[1]
    }

    /// Extracts the option name from syscall arguments
    fn optname(args: &[usize]) -> usize {
        args[2]
    }

    /// Extracts the option value pointer from syscall arguments
    fn optval(args: &[usize]) -> *const u8 {
        args[3] as *const u8
    }

    /// Extracts the option value length from syscall arguments
    fn optlen(args: &[usize]) -> usize {
        args[4]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETSOCKOPT, SysSetsockoptHandle);

/// Internal implementation of the setsockopt operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `level` - Option level
/// * `optname` - Option name
/// * `optval` - Option value
///
/// # Returns
/// * `Ok(usize)` - 0 on success
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_setsockopt(
    fd: usize,
    level: usize,
    optname: usize,
    optval: &[u8],
) -> Result<usize, SystemError> {
    let sol = socket::PSOL::try_from(level as u32)?;
    let socket = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
    socket
        .as_socket()
        .unwrap()
        .set_option(sol, optname, optval)
        .map(|_| 0)
}
