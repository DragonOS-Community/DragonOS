use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETSOCKOPT;
use crate::mm::VirtAddr;
use crate::net::socket::packet::packet_option;
use crate::net::socket::{PIPV6, PSO, PSOL};
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
        let raw_optlen = Self::optlen(args);

        // Linux resolves the descriptor before inspecting any option-specific
        // length or user pointer. Keep the inode alive through the copy so a
        // concurrent close cannot change which socket receives the option.
        let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;

        // The syscall ABI declares optlen as a 32-bit signed int. Scalar
        // register arguments are truncated to the low 32 bits; negative
        // lengths are rejected before inspecting optval.
        let signed_optlen = raw_optlen as u32 as i32;
        if signed_optlen < 0 {
            return Err(SystemError::EINVAL);
        }
        let optlen = signed_optlen as usize;

        // Linux 6.6 行为：IPV6_CHECKSUM 在 setsockopt 时会无视 optlen，直接按 int 读取。
        // gVisor raw_socket_test: RawSocketTest.SetIPv6ChecksumError_ReadShort
        let mut optlen_to_read = optlen;
        if level == PSOL::IPV6 as usize && optname == PIPV6::CHECKSUM as usize {
            optlen_to_read = core::mem::size_of::<i32>();
        }
        // Linux validates the integer length before touching optval, then
        // consumes only the leading int even when a longer buffer is supplied.
        let filter_int_option = level == PSOL::SOCKET as usize
            && matches!(
                PSO::try_from(optname as u32),
                Ok(PSO::DETACH_FILTER | PSO::LOCK_FILTER)
            );
        if filter_int_option {
            if optlen < core::mem::size_of::<i32>() {
                return Err(SystemError::EINVAL);
            }
            optlen_to_read = core::mem::size_of::<i32>();
        }
        // Linux's generic SOL_SOCKET path first reads an int. For a malformed
        // SO_ATTACH_FILTER length, preserve that access/error ordering, then
        // pass a four-byte slice so the fprog parser returns EINVAL. Only the
        // exact native layout causes the full structure to be read.
        let attach_filter = level == PSOL::SOCKET as usize
            && matches!(PSO::try_from(optname as u32), Ok(PSO::ATTACH_FILTER));
        if attach_filter {
            if optlen < core::mem::size_of::<i32>() {
                return Err(SystemError::EINVAL);
            }
            if optlen != core::mem::size_of::<crate::bpf::classic::SockFprog>() {
                optlen_to_read = core::mem::size_of::<i32>();
            }
        }

        // Linux validates exact-width packet scalars before touching optval.
        // Admit them here so malformed lengths cannot be turned into EFAULT
        // by an invalid pointer before the option handler returns EINVAL.
        let packet_exact_u32 = level == PSOL::PACKET as usize
            && matches!(
                optname,
                packet_option::PACKET_VERSION | packet_option::PACKET_RESERVE
            );
        if packet_exact_u32 {
            if optlen != core::mem::size_of::<u32>() {
                return Err(SystemError::EINVAL);
            }
            optlen_to_read = core::mem::size_of::<u32>();
        }

        // Verify optval address validity if from user space
        if frame.is_from_user() {
            let virt_optval = VirtAddr::new(optval as usize);
            if crate::mm::access_ok(virt_optval, optlen_to_read).is_err() {
                return Err(SystemError::EFAULT);
            }
        }

        // Copy optval through the exception-table protected path. access_ok()
        // only validates the address range; it does not prove that every page
        // is mapped, so directly borrowing userspace here can panic the kernel.
        let user_buffer_reader =
            UserBufferReader::new(optval, optlen_to_read, frame.is_from_user())?;
        let mut data = Vec::new();
        data.try_reserve_exact(optlen_to_read)
            .map_err(|_| SystemError::ENOMEM)?;
        data.resize(optlen_to_read, 0);
        user_buffer_reader.copy_from_user_protected(&mut data, 0)?;

        let sol = PSOL::try_from(level as u32)?;
        socket_inode
            .as_socket()
            .unwrap()
            .set_option(sol, optname, &data)
            .map(|_| 0)
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
