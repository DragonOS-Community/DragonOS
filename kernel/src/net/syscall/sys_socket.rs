use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SOCKET;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::net::posix::PosixArgsSocketType;
use crate::net::socket;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `socket` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for creating a socket.
pub struct SysSocketHandle;

impl Syscall for SysSocketHandle {
    /// Returns the number of arguments expected by the `socket` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `socket` system call
    ///
    /// Creates a new socket.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Address family (usize)
    ///   - args[1]: Socket type (usize)
    ///   - args[2]: Protocol (usize)
    /// * `frame` - Trap frame (not used)
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor of the new socket
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let address_family = Self::address_family(args);
        let socket_type = Self::socket_type(args);
        let protocol = Self::protocol(args);

        do_socket(address_family, socket_type, protocol)
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
            FormattedSyscallParam::new("domain", Self::address_family(args).to_string()),
            FormattedSyscallParam::new("type", Self::socket_type(args).to_string()),
            FormattedSyscallParam::new("protocol", Self::protocol(args).to_string()),
        ]
    }
}

impl SysSocketHandle {
    /// Extracts the address family from syscall arguments
    fn address_family(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the socket type from syscall arguments
    fn socket_type(args: &[usize]) -> usize {
        args[1]
    }

    /// Extracts the protocol from syscall arguments
    fn protocol(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_SOCKET, SysSocketHandle);

/// Internal implementation of the socket operation
///
/// # Arguments
/// * `address_family` - Address family
/// * `socket_type` - Socket type
/// * `protocol` - Protocol
///
/// # Returns
/// * `Ok(usize)` - File descriptor of the new socket
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_socket(
    address_family: usize,
    socket_type: usize,
    protocol: usize,
) -> Result<usize, SystemError> {
    let address_family = socket::AddressFamily::try_from(address_family as u16)?;
    let type_arg = PosixArgsSocketType::from_bits_truncate(socket_type as u32);
    let is_nonblock = type_arg.is_nonblock();
    let is_close_on_exec = type_arg.is_cloexec();
    let stype = socket::PSOCK::try_from(type_arg)?;

    let inode = socket::create_socket(
        address_family,
        stype,
        protocol as u32,
        is_nonblock,
        is_close_on_exec,
    )?;

    let file = File::new_socket(inode, FileFlags::O_RDWR)?;
    // 把socket添加到当前进程的文件描述符表中
    ProcessManager::current_pcb()
        .fd_table()
        .write()
        .alloc_fd(file, None, is_close_on_exec)
        .map(|x| x as usize)
}
