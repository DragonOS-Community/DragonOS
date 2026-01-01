use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RSEQ;
use crate::mm::VirtAddr;
use crate::process::rseq::Rseq;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::vec::Vec;
use system_error::SystemError;

/// System call handler for the `rseq` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// registering/unregistering restartable sequences (rseq).
pub struct SysRseqHandle;

impl SysRseqHandle {
    /// Extracts the rseq pointer from syscall arguments
    fn rseq_ptr(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[0])
    }

    /// Extracts the rseq length from syscall arguments
    fn rseq_len(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> i32 {
        args[2] as i32
    }

    /// Extracts the signature from syscall arguments
    fn sig(args: &[usize]) -> u32 {
        args[3] as u32
    }
}

impl Syscall for SysRseqHandle {
    /// Returns the number of arguments expected by the `rseq` syscall
    fn num_args(&self) -> usize {
        4
    }

    /// Handles the `rseq` system call
    ///
    /// Registers or unregisters a restartable sequence (rseq) for the current process.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Pointer to rseq structure (VirtAddr)
    ///   - args[1]: Length of rseq structure (u32)
    ///   - args[2]: Flags (i32): 0 for register, RSEQ_FLAG_UNREGISTER for unregister
    ///   - args[3]: Signature value (u32)
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let rseq_ptr = Self::rseq_ptr(args);
        let rseq_len = Self::rseq_len(args);
        let flags = Self::flags(args);
        let sig = Self::sig(args);

        Rseq::syscall(rseq_ptr, rseq_len, flags, sig)
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
            FormattedSyscallParam::new("rseq_ptr", format!("{:#x}", Self::rseq_ptr(args).data())),
            FormattedSyscallParam::new("rseq_len", Self::rseq_len(args).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
            FormattedSyscallParam::new("sig", format!("{:#x}", Self::sig(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_RSEQ, SysRseqHandle);
