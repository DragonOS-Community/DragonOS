use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::SigSet;
use crate::arch::syscall::nr::SYS_PPOLL;
use crate::filesystem::poll::{
    do_sys_poll, poll_select_finish, poll_select_set_timeout, read_pollfds_from_user,
    write_pollfds_revents_to_user, PollFd, PollTimeType,
};
use crate::ipc::signal::set_user_sigmask;
use crate::mm::VirtAddr;
use crate::process::resource::RLimitID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use crate::time::{Instant, PosixTimeSpec};
use alloc::string::ToString;
use alloc::vec::Vec;
use core::mem::size_of;
use system_error::SystemError;

/// System call handler for the `ppoll` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// polling file descriptors for events with a timespec timeout and signal mask.
pub struct SysPpollHandle;

impl SysPpollHandle {
    /// Extracts the pollfd pointer from syscall arguments
    fn pollfd_ptr(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the number of file descriptors from syscall arguments
    fn nfds(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the timespec pointer from syscall arguments
    fn timespec_ptr(args: &[usize]) -> usize {
        args[2]
    }

    /// Extracts the sigmask pointer from syscall arguments
    fn sigmask_ptr(args: &[usize]) -> usize {
        args[3]
    }

    /// Extracts the sigsetsize from syscall arguments
    fn sigsetsize(args: &[usize]) -> usize {
        args[4]
    }
}

impl Syscall for SysPpollHandle {
    /// Returns the number of arguments expected by the `ppoll` syscall
    fn num_args(&self) -> usize {
        5
    }

    /// Handles the `ppoll` system call
    ///
    /// Polls file descriptors for events with a timespec timeout and signal mask.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Pointer to pollfd array (usize)
    ///   - args[1]: Number of file descriptors (u32)
    ///   - args[2]: Pointer to timespec structure (usize)
    ///   - args[3]: Pointer to sigset_t structure (usize)
    ///   - args[4]: Size of sigset_t (usize)
    /// * `_frame` - Trap frame (unused)
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of file descriptors with events
    /// * `Err(SystemError)` - Error code if operation fails
    ///
    /// Reference: https://code.dragonos.org.cn/xref/linux-6.1.9/fs/select.c#1101
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pollfd_ptr = Self::pollfd_ptr(args);
        let nfds = Self::nfds(args);
        let timespec_ptr = Self::timespec_ptr(args);
        let sigmask_ptr = Self::sigmask_ptr(args);
        let sigsetsize = Self::sigsetsize(args);

        do_ppoll(pollfd_ptr, nfds, timespec_ptr, sigmask_ptr, sigsetsize)
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
            FormattedSyscallParam::new("timespec_ptr", format!("{:#x}", Self::timespec_ptr(args))),
            FormattedSyscallParam::new("sigmask_ptr", format!("{:#x}", Self::sigmask_ptr(args))),
            FormattedSyscallParam::new("sigsetsize", Self::sigsetsize(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PPOLL, SysPpollHandle);

/// Internal implementation of the ppoll operation
///
/// # Arguments
/// * `pollfd_ptr` - Pointer to pollfd array
/// * `nfds` - Number of file descriptors
/// * `timespec_ptr` - Pointer to timespec structure
/// * `sigmask_ptr` - Pointer to sigset_t structure
/// * `sigsetsize` - Size of sigset_t
///
/// # Returns
/// * `Ok(usize)` - Number of file descriptors with events
/// * `Err(SystemError)` - Error code if operation fails
#[inline(never)]
pub fn do_ppoll(
    pollfd_ptr: usize,
    nfds: u32,
    timespec_ptr: usize,
    sigmask_ptr: usize,
    sigsetsize: usize,
) -> Result<usize, SystemError> {
    // 检查 nfds 是否超过 RLIMIT_NOFILE
    let rlimit_nofile = ProcessManager::current_pcb()
        .get_rlimit(RLimitID::Nofile)
        .rlim_cur as u32;
    if nfds > rlimit_nofile {
        return Err(SystemError::EINVAL);
    }

    // 检查长度溢出
    let pollfds_len = (nfds as usize)
        .checked_mul(core::mem::size_of::<PollFd>())
        .ok_or(SystemError::EINVAL)?;

    // 当 nfds > 0 但 pollfd_ptr 为空指针时，返回 EFAULT
    if nfds > 0 && pollfd_ptr == 0 {
        return Err(SystemError::EFAULT);
    }

    let mut timeout_ts: Option<Instant> = None;
    let mut sigmask: Option<SigSet> = None;
    let pollfd_ptr = VirtAddr::new(pollfd_ptr);

    // 验证 sigsetsize 参数（符合 Linux 6.6 标准）
    if sigmask_ptr != 0 && sigsetsize != size_of::<SigSet>() {
        return Err(SystemError::EINVAL);
    }

    if sigmask_ptr != 0 {
        let sigmask_reader =
            UserBufferReader::new(sigmask_ptr as *const SigSet, size_of::<SigSet>(), true)?;
        sigmask = Some(sigmask_reader.buffer_protected(0)?.read_one::<SigSet>(0)?);
    }

    if timespec_ptr != 0 {
        let tsreader = UserBufferReader::new(
            timespec_ptr as *const PosixTimeSpec,
            size_of::<PosixTimeSpec>(),
            true,
        )?;
        let ts: PosixTimeSpec = tsreader.buffer_protected(0)?.read_one::<PosixTimeSpec>(0)?;

        // 根据 Linux 6.6 标准验证 timespec
        // 1. tv_sec 必须非负
        if ts.tv_sec < 0 {
            return Err(SystemError::EINVAL);
        }
        // 2. tv_nsec 必须在 [0, 999999999] 范围内（规范化检查）
        if ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
            return Err(SystemError::EINVAL);
        }

        let timeout_ms = ts.as_millis_saturating_u64();
        timeout_ts = Some(poll_select_set_timeout(timeout_ms).ok_or(SystemError::EINVAL)?);
    }

    if let Some(mut sigmask) = sigmask {
        set_user_sigmask(&mut sigmask);
    }

    // nfds == 0 时，直接进入等待逻辑，不需要用户缓冲区
    let r: Result<usize, SystemError> = if nfds == 0 {
        do_sys_poll(&mut [], timeout_ts)
    } else {
        use crate::syscall::user_access::UserBufferWriter;
        let mut poll_fds_writer =
            UserBufferWriter::new(pollfd_ptr.as_ptr::<PollFd>(), pollfds_len, true)?;
        let mut user_buf = poll_fds_writer.buffer_protected(0)?;
        let mut poll_fds = read_pollfds_from_user(&mut user_buf, nfds as usize)?;
        let mut r = do_sys_poll(&mut poll_fds, timeout_ts);
        if let Err(e) = write_pollfds_revents_to_user(&mut user_buf, &poll_fds) {
            r = Err(e);
        }
        r
    };

    return poll_select_finish(timeout_ts, timespec_ptr, PollTimeType::TimeSpec, r);
}
