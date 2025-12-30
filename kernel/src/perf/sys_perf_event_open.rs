use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PERF_EVENT_OPEN;
use crate::include::bindings::linux_bpf::perf_event_attr;
use crate::perf::perf_event_open;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::mem::size_of;
use system_error::SystemError;

/// System call handler for the `perf_event_open` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// performance event monitoring.
pub struct SysPerfEventOpenHandle;

impl SysPerfEventOpenHandle {
    /// Extracts the attribute pointer from syscall arguments
    fn attr(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the pid from syscall arguments
    fn pid(args: &[usize]) -> i32 {
        args[1] as i32
    }

    /// Extracts the cpu from syscall arguments
    fn cpu(args: &[usize]) -> i32 {
        args[2] as i32
    }

    /// Extracts the group_fd from syscall arguments
    fn group_fd(args: &[usize]) -> i32 {
        args[3] as i32
    }

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> u32 {
        args[4] as u32
    }
}

impl Syscall for SysPerfEventOpenHandle {
    /// Returns the number of arguments expected by the `perf_event_open` syscall
    fn num_args(&self) -> usize {
        5
    }

    /// Handles the `perf_event_open` system call
    ///
    /// Opens a performance event file descriptor.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Pointer to perf_event_attr structure (*const u8)
    ///   - args[1]: Process ID (i32)
    ///   - args[2]: CPU ID (i32)
    ///   - args[3]: Group file descriptor (i32)
    ///   - args[4]: Flags (u32)
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let attr = Self::attr(args);
        let pid = Self::pid(args);
        let cpu = Self::cpu(args);
        let group_fd = Self::group_fd(args);
        let flags = Self::flags(args);

        let buf = UserBufferReader::new(
            attr as *const perf_event_attr,
            size_of::<perf_event_attr>(),
            true,
        )?;
        let attr = buf.buffer_protected(0)?.read_one::<perf_event_attr>(0)?;
        perf_event_open(&attr, pid, cpu, group_fd, flags)
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
            FormattedSyscallParam::new("attr", format!("{:#x}", Self::attr(args) as usize)),
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("cpu", Self::cpu(args).to_string()),
            FormattedSyscallParam::new("group_fd", Self::group_fd(args).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PERF_EVENT_OPEN, SysPerfEventOpenHandle);
