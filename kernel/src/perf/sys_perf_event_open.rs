use crate::arch::syscall::nr::SYS_PERF_EVENT_OPEN;
use crate::arch::{MMArch, interrupt::TrapFrame};
use crate::include::bindings::linux_bpf::perf_event_attr;
use crate::mm::MemoryManagementArch;
use crate::perf::perf_event_open;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cmp::min;
use core::mem::size_of;
use system_error::SystemError;

/// System call handler for the `perf_event_open` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// performance event monitoring.
pub struct SysPerfEventOpenHandle;

const PERF_ATTR_SIZE_VER0: usize = 64;

fn report_supported_attr_size(attr: *const u8) -> Result<(), SystemError> {
    let mut writer = UserBufferWriter::new(attr as *mut u8, 8, true)?;
    writer.copy_to_user_protected(&(size_of::<perf_event_attr>() as u32).to_ne_bytes(), 4)?;
    Ok(())
}

/// Linux-compatible `perf_copy_attr`: short known versions are zero-extended,
/// while a longer userspace structure is accepted only when its unknown tail
/// is all zero.
fn copy_perf_event_attr(attr: *const u8) -> Result<perf_event_attr, SystemError> {
    let header = UserBufferReader::new(attr, 8, true)?;
    let mut size_bytes = [0u8; size_of::<u32>()];
    header.copy_from_user_protected(&mut size_bytes, 4)?;
    let reported_size = u32::from_ne_bytes(size_bytes) as usize;
    let user_size = if reported_size == 0 {
        PERF_ATTR_SIZE_VER0
    } else {
        reported_size
    };

    if !(PERF_ATTR_SIZE_VER0..=MMArch::PAGE_SIZE).contains(&user_size) {
        report_supported_attr_size(attr)?;
        return Err(SystemError::E2BIG);
    }

    let reader = UserBufferReader::new(attr, user_size, true)?;
    let mut kernel_attr: perf_event_attr = unsafe { core::mem::zeroed() };
    let local_size = size_of::<perf_event_attr>();
    let copy_size = min(user_size, local_size);
    let attr_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut kernel_attr as *mut perf_event_attr).cast::<u8>(),
            local_size,
        )
    };
    reader.copy_from_user_protected(&mut attr_bytes[..copy_size], 0)?;

    if user_size > local_size {
        let mut unknown_tail = alloc::vec![0u8; user_size - local_size];
        reader.copy_from_user_protected(&mut unknown_tail, local_size)?;
        if unknown_tail.iter().any(|byte| *byte != 0) {
            report_supported_attr_size(attr)?;
            return Err(SystemError::E2BIG);
        }
    }

    kernel_attr.size = user_size as u32;
    Ok(kernel_attr)
}

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
    fn flags(args: &[usize]) -> usize {
        args[4]
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

        let attr = copy_perf_event_attr(attr)?;
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
