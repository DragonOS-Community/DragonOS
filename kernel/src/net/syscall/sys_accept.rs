use system_error::SystemError;

use super::socket_fd::SocketFdRef;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_ACCEPT;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::net::posix::SockAddr;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `accept` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for accepting a connection on a socket.
pub struct SysAcceptHandle;

impl Syscall for SysAcceptHandle {
    /// Returns the number of arguments expected by the `accept` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `accept` system call
    ///
    /// Accepts a connection on a socket.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Address pointer (*mut SockAddr) - may be null
    ///   - args[2]: Address length pointer (*mut u32) - may be null
    /// * `frame` - Trap frame (not used)
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor of the accepted socket
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let addr = Self::addr(args);
        let addrlen = Self::addrlen(args);

        do_accept(fd, addr, addrlen, 0)
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
            FormattedSyscallParam::new("addr", format!("{:#x}", Self::addr(args) as usize)),
            FormattedSyscallParam::new("addrlen", format!("{:#x}", Self::addrlen(args) as usize)),
        ]
    }
}

impl SysAcceptHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the address pointer from syscall arguments
    fn addr(args: &[usize]) -> *mut SockAddr {
        args[1] as *mut SockAddr
    }

    /// Extracts the address length pointer from syscall arguments
    fn addrlen(args: &[usize]) -> *mut u32 {
        args[2] as *mut u32
    }
}

syscall_table_macros::declare_syscall!(SYS_ACCEPT, SysAcceptHandle);

/// Internal implementation of the accept operation
///
/// This function is shared by both accept and accept4.
///
/// # Arguments
/// * `fd` - File descriptor
/// * `addr` - Address pointer (may be null)
/// * `addrlen` - Address length pointer (may be null)
/// * `flags` - Flags for accept4 (0 for accept)
///
/// # Returns
/// * `Ok(usize)` - File descriptor of the accepted socket
/// * `Err(SystemError)` - Error code if operation fails
pub(crate) fn do_accept(
    fd: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
    flags: u32,
) -> Result<usize, SystemError> {
    // Hold the open file description across the (possibly blocking) accept,
    // matching Linux `__sys_accept4()` (`net/socket.c`, which pairs `fdget()`
    // with `fdput()`): without it a concurrent `close(listener)` tears the
    // socket down mid-wait.
    let sock = SocketFdRef::from_fd(fd as i32)?;

    let mut file_mode = FileFlags::O_RDWR;
    if flags & FileFlags::O_NONBLOCK.bits() != 0 {
        file_mode |= FileFlags::O_NONBLOCK;
    }
    if flags & FileFlags::O_CLOEXEC.bits() != 0 {
        file_mode |= FileFlags::O_CLOEXEC;
    }

    let cloexec = flags & FileFlags::O_CLOEXEC.bits() != 0;

    let current = ProcessManager::current_pcb();

    // Reserve the descriptor number *before* waiting, exactly like
    // `__sys_accept4_file()` (`net/socket.c`: `get_unused_fd_flags()` precedes
    // `do_accept()`): a full descriptor table reports `EMFILE` without blocking
    // and without consuming a queued connection. A reserved slot is invisible
    // (`EBADF` to `close()`, `EBUSY` to `dup2()`), so it cannot be recycled
    // while the peer address is copied below. `Drop` releases it on every error
    // path; `install()` publishes the file once the syscall is ready to return.
    let reservation = current
        .fd_table()
        .reserve::<1>(current.nofile_soft_limit(), 0, cloexec)?;

    let (new_socket, remote_endpoint) = sock.socket()?.accept()?;
    let new_file = File::new_socket(new_socket, file_mode)?;

    if !addr.is_null() {
        // Copy the peer address into user space.
        //
        // Linux copies the peer address *before* installing the new descriptor
        // (`do_accept()` runs `move_addr_to_user()` before `fd_install()`).
        // Reproducing that order here makes a faulting user pointer close the
        // accepted socket and report `EFAULT`, instead of leaking one
        // descriptor plus one live connection per failed `accept()`. Moving the
        // copy first also means `EMFILE` is reported before `EFAULT`, as in
        // Linux.
        remote_endpoint.write_to_user(addr, addrlen)?;
    }

    // `install()` performs the one step that can still fail, allocating the
    // `Arc<File>` (`ENOMEM`); on that path the reservation's `Drop` releases the
    // reserved number, so the descriptor table is left untouched.
    Ok(reservation.install(new_file)? as usize)
}
