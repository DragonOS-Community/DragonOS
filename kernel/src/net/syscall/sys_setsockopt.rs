use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETSOCKOPT;
use crate::net::socket::inet::stream::TcpOption;
use crate::net::socket::packet::packet_option;
use crate::net::socket::{IpOption, IFNAMSIZ, PIPV6, PSO, PSOL};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `setsockopt` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for setting socket options.
pub struct SysSetsockoptHandle;

#[derive(Debug, Clone, Copy)]
struct OptvalCopySpec {
    len: usize,
    fault_is_einval: bool,
    stop_at_nul: bool,
}

impl OptvalCopySpec {
    const fn bytes(len: usize) -> Self {
        Self {
            len,
            fault_is_einval: false,
            stop_at_nul: false,
        }
    }

    const fn bytes_with_einval_fault(len: usize) -> Self {
        Self {
            len,
            fault_is_einval: true,
            stop_at_nul: false,
        }
    }

    const fn c_string(len: usize) -> Self {
        Self {
            len,
            fault_is_einval: false,
            stop_at_nul: true,
        }
    }

    const fn fault_error(self) -> SystemError {
        if self.fault_is_einval {
            SystemError::EINVAL
        } else {
            SystemError::EFAULT
        }
    }
}

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

        let sol = PSOL::try_from(level as u32)?;
        let copy_spec = Self::optval_copy_spec(sol, optname, optlen)?;
        let mut storage = [0u8; Self::MAX_OPTVAL_COPY];
        let copied = if copy_spec.stop_at_nul {
            // Linux's string options stop at the first NUL. Read one protected
            // byte at a time so an inaccessible tail after that terminator is
            // never touched.
            let mut copied = 0;
            for (offset, byte) in storage[..copy_spec.len].iter_mut().enumerate() {
                let address = (optval as usize)
                    .checked_add(offset)
                    .ok_or(copy_spec.fault_error())?;
                let reader = UserBufferReader::new(address as *const u8, 1, frame.is_from_user())
                    .map_err(|_| copy_spec.fault_error())?;
                reader
                    .copy_from_user_protected(core::slice::from_mut(byte), 0)
                    .map_err(|_| copy_spec.fault_error())?;
                copied += 1;
                if *byte == 0 {
                    break;
                }
            }
            copied
        } else if copy_spec.len == 0 {
            0
        } else {
            // Copy through the exception-table protected path. The address
            // range check alone does not prove that every page is mapped.
            let reader = UserBufferReader::new(optval, copy_spec.len, frame.is_from_user())
                .map_err(|_| copy_spec.fault_error())?;
            reader
                .copy_from_user_protected(&mut storage[..copy_spec.len], 0)
                .map_err(|_| copy_spec.fault_error())?;
            copy_spec.len
        };
        let data = &storage[..copied];

        socket_inode
            .as_socket()
            .unwrap()
            .set_option(sol, optname, data)
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
    /// Largest fixed UAPI object copied by a currently supported option
    /// (Linux's 40-byte internal `packet_mreq_max`). Keeping it on the stack makes
    /// userspace `optlen` incapable of driving kernel heap allocation.
    const MAX_OPTVAL_COPY: usize = 8 + 32;

    /// Return the prefix that the selected Linux 6.6 option actually consumes.
    /// Exact-width options are validated here so their EINVAL-before-EFAULT
    /// ordering is preserved. Add new non-scalar option layouts to this table
    /// together with their socket-level parser.
    fn optval_copy_spec(
        level: PSOL,
        optname: usize,
        optlen: usize,
    ) -> Result<OptvalCopySpec, SystemError> {
        const INT_LEN: usize = core::mem::size_of::<i32>();
        const TIMEVAL_LEN: usize = 16;
        const LINGER_LEN: usize = 8;
        const IPV4_MREQN_LEN: usize = 12;
        const IPV6_MREQ_LEN: usize = 20;
        const PACKET_MREQ_LEN: usize = 16;
        const PACKET_MREQ_MAX_LEN: usize = 40;
        const TPACKET_REQ_LEN: usize = 16;
        const TCP_CA_NAME_MAX: usize = 16;

        let spec = match level {
            PSOL::SOCKET => match PSO::try_from(optname as u32) {
                Ok(PSO::BINDTODEVICE) => OptvalCopySpec::bytes(optlen.min(IFNAMSIZ - 1)),
                _ if optlen < INT_LEN => return Err(SystemError::EINVAL),
                Ok(PSO::DETACH_FILTER | PSO::LOCK_FILTER) => OptvalCopySpec::bytes(INT_LEN),
                Ok(PSO::ATTACH_FILTER) => {
                    let fprog_len = core::mem::size_of::<crate::bpf::classic::SockFprog>();
                    if optlen == fprog_len {
                        OptvalCopySpec::bytes(fprog_len)
                    } else {
                        OptvalCopySpec::bytes(INT_LEN)
                    }
                }
                Ok(PSO::LINGER) => OptvalCopySpec::bytes(if optlen < LINGER_LEN {
                    INT_LEN
                } else {
                    LINGER_LEN
                }),
                Ok(
                    PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW | PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW,
                ) => OptvalCopySpec::bytes(if optlen < TIMEVAL_LEN {
                    INT_LEN
                } else {
                    TIMEVAL_LEN
                }),
                _ => OptvalCopySpec::bytes(INT_LEN),
            },
            PSOL::IP => match IpOption::try_from(optname as u32) {
                Ok(
                    IpOption::MULTICAST_IF | IpOption::ADD_MEMBERSHIP | IpOption::DROP_MEMBERSHIP,
                ) => OptvalCopySpec::bytes(optlen.min(IPV4_MREQN_LEN)),
                _ => OptvalCopySpec::bytes(optlen.min(INT_LEN)),
            },
            PSOL::IPV6 => {
                if optname == PIPV6::CHECKSUM as usize {
                    if optlen < INT_LEN {
                        return Err(SystemError::EINVAL);
                    }
                    OptvalCopySpec::bytes(INT_LEN)
                } else if matches!(
                    PIPV6::try_from(optname as u32),
                    Ok(PIPV6::ADD_MEMBERSHIP | PIPV6::DROP_MEMBERSHIP)
                ) {
                    OptvalCopySpec::bytes(optlen.min(IPV6_MREQ_LEN))
                } else {
                    OptvalCopySpec::bytes(optlen.min(INT_LEN))
                }
            }
            PSOL::TCP
                if matches!(
                    TcpOption::try_from(optname as i32),
                    Ok(TcpOption::Congestion | TcpOption::ULP)
                ) =>
            {
                if optlen < 1 {
                    return Err(SystemError::EINVAL);
                }
                OptvalCopySpec::c_string(optlen.min(TCP_CA_NAME_MAX - 1))
            }
            PSOL::TCP => {
                if optlen < INT_LEN {
                    return Err(SystemError::EINVAL);
                }
                OptvalCopySpec::bytes(INT_LEN)
            }
            PSOL::PACKET => match optname {
                packet_option::PACKET_VERSION | packet_option::PACKET_RESERVE => {
                    if optlen != INT_LEN {
                        return Err(SystemError::EINVAL);
                    }
                    OptvalCopySpec::bytes(INT_LEN)
                }
                packet_option::PACKET_FANOUT => {
                    if optlen != INT_LEN && optlen != 2 * INT_LEN {
                        return Err(SystemError::EINVAL);
                    }
                    OptvalCopySpec::bytes(optlen)
                }
                packet_option::PACKET_ADD_MEMBERSHIP | packet_option::PACKET_DROP_MEMBERSHIP => {
                    if optlen < PACKET_MREQ_LEN {
                        return Err(SystemError::EINVAL);
                    }
                    OptvalCopySpec::bytes(optlen.min(PACKET_MREQ_MAX_LEN))
                }
                packet_option::PACKET_RX_RING => {
                    if optlen < TPACKET_REQ_LEN {
                        return Err(SystemError::EINVAL);
                    }
                    // Linux maps tpacket_req copy faults to EINVAL.
                    OptvalCopySpec::bytes_with_einval_fault(TPACKET_REQ_LEN)
                }
                packet_option::PACKET_AUXDATA => {
                    if optlen < INT_LEN {
                        return Err(SystemError::EINVAL);
                    }
                    OptvalCopySpec::bytes(INT_LEN)
                }
                _ => OptvalCopySpec::bytes(optlen.min(INT_LEN)),
            },
            PSOL::ICMPV6 if optname == 1 => {
                OptvalCopySpec::bytes(optlen.min(core::mem::size_of::<[u32; 8]>()))
            }
            _ => OptvalCopySpec::bytes(optlen.min(INT_LEN)),
        };
        if spec.len > Self::MAX_OPTVAL_COPY {
            return Err(SystemError::EINVAL);
        }
        Ok(spec)
    }

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
