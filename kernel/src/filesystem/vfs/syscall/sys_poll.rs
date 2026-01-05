use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_POLL;
use crate::filesystem::poll::{do_sys_poll, poll_select_set_timeout, PollFd, RestartFnPoll};
use crate::ipc::signal::{RestartBlock, RestartBlockData};
use crate::mm::VirtAddr;
use crate::process::resource::RLimitID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use crate::time::Instant;
use alloc::string::ToString;
use alloc::vec::Vec;
use system_error::SystemError;

/// System call handler for the `poll` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// polling file descriptors for events.
pub struct SysPollHandle;

impl SysPollHandle {
    /// Extracts the pollfd pointer from syscall arguments
    fn pollfd_ptr(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the number of file descriptors from syscall arguments
    fn nfds(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the timeout from syscall arguments
    fn timeout_ms(args: &[usize]) -> i32 {
        args[2] as i32
    }
}

impl Syscall for SysPollHandle {
    /// Returns the number of arguments expected by the `poll` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `poll` system call
    ///
    /// Polls file descriptors for events.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Pointer to pollfd array (usize)
    ///   - args[1]: Number of file descriptors (u32)
    ///   - args[2]: Timeout in milliseconds (i32)
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of file descriptors with events
    /// * `Err(SystemError)` - Error code if operation fails
    ///
    /// Reference: https://code.dragonos.org.cn/xref/linux-6.6.21/fs/select.c#1068
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pollfd_ptr = Self::pollfd_ptr(args);
        let nfds = Self::nfds(args);
        let timeout_ms = Self::timeout_ms(args);

        do_poll(pollfd_ptr, nfds, timeout_ms)
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
            FormattedSyscallParam::new("pollfd_ptr", format!("{:#x}", Self::pollfd_ptr(args))),
            FormattedSyscallParam::new("nfds", Self::nfds(args).to_string()),
            FormattedSyscallParam::new("timeout_ms", Self::timeout_ms(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_POLL, SysPollHandle);

/// Internal implementation of the poll operation
///
/// # Arguments
/// * `pollfd_ptr` - Pointer to pollfd array
/// * `nfds` - Number of file descriptors
/// * `timeout_ms` - Timeout in milliseconds
///
/// # Returns
/// * `Ok(usize)` - Number of file descriptors with events
/// * `Err(SystemError)` - Error code if operation fails
#[inline(never)]
pub fn do_poll(pollfd_ptr: usize, nfds: u32, timeout_ms: i32) -> Result<usize, SystemError> {
    // 检查 nfds 是否超过 RLIMIT_NOFILE
    let rlimit_nofile = ProcessManager::current_pcb()
        .get_rlimit(RLimitID::Nofile)
        .rlim_cur as u32;
    if nfds > rlimit_nofile {
        return Err(SystemError::EINVAL);
    }

    // 检查长度溢出
    let len = (nfds as usize)
        .checked_mul(core::mem::size_of::<PollFd>())
        .ok_or(SystemError::EINVAL)?;

    // 当 nfds > 0 但 pollfd_ptr 为空指针时，返回 EFAULT
    if nfds > 0 && pollfd_ptr == 0 {
        return Err(SystemError::EFAULT);
    }

    let pollfd_ptr = VirtAddr::new(pollfd_ptr);

    let mut timeout: Option<Instant> = None;
    if timeout_ms >= 0 {
        timeout = poll_select_set_timeout(timeout_ms as u64);
    }

    // nfds == 0 时，直接进入等待逻辑，不需要用户缓冲区
    if nfds == 0 {
        let mut r = do_sys_poll(&mut [], timeout);
        if let Err(SystemError::ERESTARTNOHAND) = r {
            let restart_block_data = RestartBlockData::new_poll(pollfd_ptr, nfds, timeout);
            let restart_block = RestartBlock::new(&RestartFnPoll, restart_block_data);
            r = ProcessManager::current_pcb().set_restart_fn(Some(restart_block));
        }
        return r;
    }

    let mut poll_fds_writer = UserBufferWriter::new(pollfd_ptr.as_ptr::<PollFd>(), len, true)?;
    let mut r = do_sys_poll(poll_fds_writer.buffer(0)?, timeout);
    if let Err(SystemError::ERESTARTNOHAND) = r {
        let restart_block_data = RestartBlockData::new_poll(pollfd_ptr, nfds, timeout);
        let restart_block = RestartBlock::new(&RestartFnPoll, restart_block_data);
        r = ProcessManager::current_pcb().set_restart_fn(Some(restart_block));
    }

    return r;
}
