use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_BPF;
use crate::bpf::bpf;
use crate::include::bindings::linux_bpf::{bpf_attr, bpf_cmd};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;
use num_traits::FromPrimitive;
use system_error::SystemError;

/// System call handler for the `bpf` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// Berkeley Packet Filter (eBPF) operations.
pub struct SysBpfHandle;

impl SysBpfHandle {
    /// Extracts the command from syscall arguments
    fn cmd(args: &[usize]) -> u32 {
        args[0] as u32
    }

    /// Extracts the attribute pointer from syscall arguments
    fn attr(args: &[usize]) -> *mut u8 {
        args[1] as *mut u8
    }

    /// Extracts the attribute size from syscall arguments
    fn size(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

impl Syscall for SysBpfHandle {
    /// Returns the number of arguments expected by the `bpf` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `bpf` system call
    ///
    /// Performs various eBPF operations based on the command.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Command (u32)
    ///   - args[1]: Pointer to bpf_attr structure (*mut u8)
    ///   - args[2]: Size of bpf_attr structure (u32)
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - Result value on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let cmd = Self::cmd(args);
        let attr = Self::attr(args);
        let size = Self::size(args);

        let buf = UserBufferReader::new(attr, size as usize, true)?;
        let attr_value = buf.buffer_protected(0)?.read_one::<bpf_attr>(0)?;
        let cmd = bpf_cmd::from_u32(cmd).ok_or(SystemError::EINVAL)?;
        bpf(cmd, &attr_value)
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
            FormattedSyscallParam::new("cmd", Self::cmd(args).to_string()),
            FormattedSyscallParam::new("attr", format!("{:#x}", Self::attr(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_BPF, SysBpfHandle);
