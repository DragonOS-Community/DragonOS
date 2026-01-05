use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SOCKETPAIR;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::net::posix::PosixArgsSocketType;
use crate::net::socket;
use crate::net::socket::{unix::datagram::UnixDatagramSocket, unix::stream::UnixStreamSocket};
use crate::net::socket::{AddressFamily, PSOCK};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;

/// System call handler for the `socketpair` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for creating a pair of connected sockets.
pub struct SysSocketpairHandle;

impl Syscall for SysSocketpairHandle {
    /// Returns the number of arguments expected by the `socketpair` syscall
    fn num_args(&self) -> usize {
        4
    }

    /// Handles the `socketpair` system call
    ///
    /// Creates a pair of connected sockets.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Domain (usize)
    ///   - args[1]: Type (usize)
    ///   - args[2]: Protocol (usize)
    ///   - args[3]: File descriptors array pointer (*mut c_int)
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let domain = Self::domain(args);
        let socket_type = Self::socket_type(args);
        let protocol = Self::protocol(args);
        let fds = Self::fds(args);

        // Create UserBufferWriter for the fds array
        let mut user_buffer_writer = UserBufferWriter::new(
            fds,
            core::mem::size_of::<[c_int; 2]>(),
            frame.is_from_user(),
        )?;
        let fds_slice = user_buffer_writer.buffer::<i32>(0)?;

        do_socketpair(domain, socket_type, protocol, fds_slice)
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
            FormattedSyscallParam::new("domain", Self::domain(args).to_string()),
            FormattedSyscallParam::new("type", Self::socket_type(args).to_string()),
            FormattedSyscallParam::new("protocol", Self::protocol(args).to_string()),
            FormattedSyscallParam::new("fds", format!("{:#x}", Self::fds(args) as usize)),
        ]
    }
}

impl SysSocketpairHandle {
    /// Extracts the domain from syscall arguments
    fn domain(args: &[usize]) -> usize {
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

    /// Extracts the file descriptors array pointer from syscall arguments
    fn fds(args: &[usize]) -> *mut c_int {
        args[3] as *mut c_int
    }
}

syscall_table_macros::declare_syscall!(SYS_SOCKETPAIR, SysSocketpairHandle);

/// Internal implementation of the socketpair operation
///
/// # Arguments
/// * `address_family` - Address family
/// * `socket_type` - Socket type
/// * `protocol` - Protocol
/// * `fds` - File descriptors array (output)
///
/// # Returns
/// * `Ok(usize)` - 0 on success
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_socketpair(
    address_family: usize,
    socket_type: usize,
    protocol: usize,
    fds: &mut [i32],
) -> Result<usize, SystemError> {
    let address_family = AddressFamily::try_from(address_family as u16)?;
    let socket_type = PosixArgsSocketType::from_bits_truncate(socket_type as u32);
    let stype = socket::PSOCK::try_from(socket_type)?;

    let binding = ProcessManager::current_pcb().fd_table();
    let mut fd_table_guard = binding.write();

    // check address family, only support AF_UNIX
    if address_family != AddressFamily::Unix {
        log::warn!(
            "only support AF_UNIX, {:?} with protocol {:?} is not supported",
            address_family,
            protocol
        );
        return Err(SystemError::EAFNOSUPPORT);
    }

    // Linux: if protocol is non-zero and not PF_UNIX/AF_UNIX, return EPROTONOSUPPORT.
    if protocol != 0 && protocol != AddressFamily::Unix as usize {
        return Err(SystemError::EPROTONOSUPPORT);
    }

    let nonblocking = socket_type.contains(PosixArgsSocketType::NONBLOCK);

    let (socket_a, socket_b): (
        alloc::sync::Arc<dyn socket::Socket>,
        alloc::sync::Arc<dyn socket::Socket>,
    ) = match (address_family, stype) {
        (AddressFamily::Unix, PSOCK::Stream) => {
            let (a, b) = UnixStreamSocket::new_pair(nonblocking, false);
            (a, b)
        }
        (AddressFamily::Unix, PSOCK::SeqPacket) => {
            let (a, b) = UnixStreamSocket::new_pair(nonblocking, true);
            (a, b)
        }
        (AddressFamily::Unix, PSOCK::Datagram) => {
            let (a, b) = UnixDatagramSocket::new_pair(nonblocking);
            (a, b)
        }
        // Linux supports AF_UNIX + SOCK_RAW and maps it to SOCK_DGRAM.
        (AddressFamily::Unix, PSOCK::Raw) => {
            let (a, b) = UnixDatagramSocket::new_pair(nonblocking);
            (a, b)
        }
        _ => {
            return Err(SystemError::ESOCKTNOSUPPORT);
        }
    };

    fds[0] = fd_table_guard.alloc_fd(File::new_socket(socket_a, FileFlags::O_RDWR)?, None)?;
    fds[1] = fd_table_guard.alloc_fd(File::new_socket(socket_b, FileFlags::O_RDWR)?, None)?;

    drop(fd_table_guard);
    Ok(0)
}
