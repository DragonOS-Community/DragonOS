use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_BPF;
use crate::arch::MMArch;
use crate::bpf::bpf;
use crate::include::bindings::linux_bpf::{bpf_attr, bpf_attr__bindgen_ty_4, bpf_cmd};
use crate::mm::MemoryManagementArch;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use num_traits::FromPrimitive;
use system_error::SystemError;

/// System call handler for the `bpf` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// Berkeley Packet Filter (eBPF) operations.
pub struct SysBpfHandle;

// The generated binding includes post-6.6 fields such as prog_token_fd.
// Linux 6.6's union ends at prog_load.log_true_size (144 bytes).
const BPF_ATTR_SIZE_6_6: usize =
    offset_of!(bpf_attr__bindgen_ty_4, log_true_size) + size_of::<u32>();
const _: () = assert!(BPF_ATTR_SIZE_6_6 == 144);

impl SysBpfHandle {
    /// The bpf_attr argument is versioned by `size`: missing bytes are zero,
    /// while unknown trailing bytes must be zero (Linux 6.6 __sys_bpf).
    fn read_attr(attr: *mut u8, size: usize) -> Result<bpf_attr, SystemError> {
        if size > MMArch::PAGE_SIZE {
            return Err(SystemError::E2BIG);
        }

        // A zero-length request must not construct a slice from a null pointer.
        let mut value: bpf_attr = unsafe { core::mem::zeroed() };
        if size == 0 {
            return Ok(value);
        }

        let reader = UserBufferReader::new(attr, size, true)?;
        let known_size = BPF_ATTR_SIZE_6_6;

        // Linux checks the unknown tail before copying the known prefix.
        let mut offset = known_size;
        let mut chunk = [0u8; 64];
        while offset < size {
            let len = core::cmp::min(chunk.len(), size - offset);
            reader.copy_from_user_protected(&mut chunk[..len], offset)?;
            if chunk[..len].iter().any(|byte| *byte != 0) {
                return Err(SystemError::E2BIG);
            }
            offset += len;
        }

        let copy_len = core::cmp::min(size, known_size);
        let dst = unsafe {
            core::slice::from_raw_parts_mut((&mut value as *mut bpf_attr).cast::<u8>(), copy_len)
        };
        reader.copy_from_user_protected(dst, 0)?;
        Ok(value)
    }

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

        let attr_value = Self::read_attr(attr, size as usize)?;
        let cmd = bpf_cmd::from_u32(cmd).ok_or(SystemError::EINVAL)?;
        bpf(cmd, &attr_value, attr)
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
