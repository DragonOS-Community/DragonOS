use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETRANDOM;
use crate::libs::rand::{read_random_output, GRandFlags};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_buffer::UserBuffer;
use alloc::string::ToString;
use alloc::vec::Vec;
use system_error::SystemError;

/// System call handler for the `getrandom` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// generating random bytes.
pub struct SysGetRandomHandle;

impl SysGetRandomHandle {
    /// Extracts the buffer pointer from syscall arguments
    fn buf(args: &[usize]) -> *mut u8 {
        args[0] as *mut u8
    }

    /// Extracts the buffer length from syscall arguments
    fn len(args: &[usize]) -> usize {
        args[1]
    }

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

impl Syscall for SysGetRandomHandle {
    /// Returns the number of arguments expected by the `getrandom` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `getrandom` system call
    ///
    /// Fills a buffer with random bytes.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Buffer pointer (*mut u8)
    ///   - args[1]: Buffer length (usize)
    ///   - args[2]: Flags (u32): GRND_NONBLOCK, GRND_RANDOM, GRND_INSECURE
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written on success
    /// * `Err(SystemError)` - Error code if operation fails
    ///
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let buf = Self::buf(args);
        let len = Self::len(args);
        let raw_flags = Self::flags(args);
        if raw_flags & !(GRandFlags::all().bits() as u32) != 0 {
            return Err(SystemError::EINVAL);
        }
        let flags = GRandFlags::from_bits(raw_flags as u8).ok_or(SystemError::EINVAL)?;

        do_get_random(buf, len, flags)
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
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETRANDOM, SysGetRandomHandle);

/// Internal implementation of the getrandom operation
///
/// # Arguments
/// * `buf` - Buffer to fill with random bytes
/// * `len` - Length of buffer
/// * `flags` - Flags (GRND_NONBLOCK, GRND_RANDOM, GRND_INSECURE)
///
/// # Returns
/// * `Ok(usize)` - Number of bytes written
/// * `Err(SystemError)` - Error code if operation fails
///
pub fn do_get_random(buf: *mut u8, len: usize, flags: GRandFlags) -> Result<usize, SystemError> {
    if flags.contains(GRandFlags::GRND_INSECURE) && flags.contains(GRandFlags::GRND_RANDOM) {
        return Err(SystemError::EINVAL);
    }
    let len = len.min(crate::filesystem::vfs::MAX_RW_COUNT);
    if len == 0 {
        return Ok(0);
    }
    let mut writer = UserBuffer::new_protected(buf, len, true)?;
    read_random_output(
        len,
        flags.contains(GRandFlags::GRND_INSECURE),
        flags.contains(GRandFlags::GRND_NONBLOCK),
        |offset, bytes| writer.write_prefix_to_user(offset, bytes),
    )
}
