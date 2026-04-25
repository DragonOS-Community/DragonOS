use crate::arch::interrupt::TrapFrame;
use crate::arch::rand::rand;
use crate::arch::syscall::nr::SYS_GETRANDOM;
use crate::libs::rand::GRandFlags;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cmp;
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
    fn flags(args: &[usize]) -> u8 {
        args[2] as u8
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
    ///   - args[2]: Flags (u8): GRND_NONBLOCK, GRND_RANDOM, GRND_INSECURE
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written on success
    /// * `Err(SystemError)` - Error code if operation fails
    ///
    /// Note: This syscall implementation differs from Linux as there is currently
    /// no other random source available.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let buf = Self::buf(args);
        let len = Self::len(args);
        let flags = GRandFlags::from_bits(Self::flags(args)).ok_or(SystemError::EINVAL)?;

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
/// Note: This implementation differs from Linux as there is currently no other random source.
pub fn do_get_random(buf: *mut u8, len: usize, flags: GRandFlags) -> Result<usize, SystemError> {
    if flags.bits() == (GRandFlags::GRND_INSECURE.bits() | GRandFlags::GRND_RANDOM.bits()) {
        return Err(SystemError::EINVAL);
    }

    let mut writer = UserBufferWriter::new(buf, len, true)?;
    let mut buffer = writer.buffer_protected(0)?;

    let mut count = 0;
    while count < len {
        // 对 len - count 的长度进行判断，remain_len 小于4则循环次数和 remain_len 相等
        let remain_len = len - count;
        let step = cmp::min(remain_len, 4);
        let rand_value = rand();

        // 生成随机字节并直接写入用户缓冲区
        let mut random_bytes = [0u8; 4];
        for (offset, byte) in random_bytes.iter_mut().enumerate().take(step) {
            *byte = (rand_value >> (offset * 2)) as u8;
        }

        // 使用异常表保护的方式写入用户缓冲区
        buffer.write_to_user(count, &random_bytes[..step])?;
        count += step;
    }

    Ok(len)
}
