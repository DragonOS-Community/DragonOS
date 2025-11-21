//! System call handler for rt_sigtimedwait.
//!
//! This module implements the rt_sigtimedwait system call, which allows a process
//! to wait for specific signals with an optional timeout.

use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::{SigSet, Signal};
use crate::arch::syscall::nr::SYS_RT_SIGTIMEDWAIT;
use crate::ipc::signal::{restore_saved_sigmask, set_user_sigmask};
use crate::ipc::signal_types::{PosixSigInfo, SigInfo};
use crate::process::preempt::PreemptGuard;
use crate::process::ProcessManager;
use crate::syscall::{
    table::{FormattedSyscallParam, Syscall},
    user_access::{UserBufferReader, UserBufferWriter},
};
use crate::time::{jiffies::NSEC_PER_JIFFY, timer::schedule_timeout, PosixTimeSpec};
use alloc::vec::Vec;
use core::mem::size_of;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysRtSigtimedwaitHandle;

/// 实现 rt_sigtimedwait 系统调用的核心逻辑
///
/// ## 参数
///
/// - `uthese`: 要等待的信号集合指针
/// - `uinfo`: 用于返回信号信息的 siginfo 结构体指针（可为 NULL）
/// - `uts`: 超时时间指针（可为 NULL 表示无限等待）
/// - `sigsetsize`: 信号集大小
/// - `from_user`: 是否来自用户态
///
/// ## 返回值
///
/// - `Ok(sig_num)`: 捕获到的信号号
/// - `Err(SystemError::EAGAIN)`: 超时
/// - `Err(SystemError::EINTR)`: 被非目标信号打断
/// - `Err(SystemError::EINVAL)`: 参数错误
/// - `Err(SystemError::EFAULT)`: 用户空间访问错误
pub fn do_kernel_rt_sigtimedwait(
    uthese: *const SigSet,
    uinfo: *mut PosixSigInfo,
    uts: *const PosixTimeSpec,
    sigsetsize: usize,
    from_user: bool,
) -> Result<usize, SystemError> {
    // 验证 sigsetsize 参数
    if sigsetsize != size_of::<SigSet>() {
        return Err(SystemError::EINVAL);
    }

    // 从用户空间读取信号集合
    let these = if uthese.is_null() {
        return Err(SystemError::EINVAL);
    } else {
        let reader = UserBufferReader::new(uthese, size_of::<SigSet>(), from_user)?;
        let sigset = reader.read_one_from_user::<SigSet>(0)?;
        // 移除不可屏蔽的信号（SIGKILL 和 SIGSTOP）
        let sigset_val: SigSet = SigSet::from_bits(sigset.bits()).ok_or(SystemError::EINVAL)?;
        let kill_stop_mask: SigSet =
            SigSet::from_bits_truncate((Signal::SIGKILL as u64) | (Signal::SIGSTOP as u64));
        let result = sigset_val & !kill_stop_mask;

        result
    };

    // 构造等待/屏蔽语义：与Linux一致
    // - 等待集合 these
    // - 临时屏蔽集合 = 旧blocked ∪ these（将这些信号作为masked的常规语义，但仍由本系统调用专门消费）
    let awaited = these;

    // 快速路径：先尝试从队列中获取信号
    if let Some((sig, info)) = try_dequeue_signal(&awaited) {
        // 如果用户提供了 uinfo 指针，拷贝信号信息
        if !uinfo.is_null() {
            copy_posix_siginfo_to_user(uinfo, &info, from_user)?;
        }
        return Ok(sig as usize);
    }

    // 设置新的信号掩码并等待
    let pcb = ProcessManager::current_pcb();
    let mut new_blocked = *pcb.sig_info_irqsave().sig_blocked();
    // 按Linux：等待期间屏蔽 these
    new_blocked.insert(awaited);
    set_user_sigmask(&mut new_blocked);

    // 计算超时时间
    let deadline = if uts.is_null() {
        None // 无限等待
    } else {
        let reader = UserBufferReader::new(uts, size_of::<PosixTimeSpec>(), from_user)?;
        let timeout = reader.read_one_from_user::<PosixTimeSpec>(0)?;
        let deadline = compute_deadline(*timeout)?;
        Some(deadline)
    };

    // 等待循环（prepare-to-wait 语义）
    let mut _loop_count = 0;
    loop {
        _loop_count += 1;

        // 第一步：准备进入可中断阻塞，先将当前线程标记为可中断阻塞，关闭中断，避免错过唤醒窗口
        let preempt_guard = PreemptGuard::new();
        // 第二步：在“已是可中断阻塞状态”下，重检是否已有目标信号，若有则不睡眠，恢复为 runnable 并返回
        if has_pending_awaited_signal(&awaited) {
            drop(preempt_guard);

            if let Some((sig, info)) = try_dequeue_signal(&awaited) {
                restore_saved_sigmask();
                if !uinfo.is_null() {
                    copy_posix_siginfo_to_user(uinfo, &info, from_user)?;
                }
                return Ok(sig as usize);
            }
            // 有 pending 但未从 awaited 集取到（被其他信号打断）
            restore_saved_sigmask();
            return Err(SystemError::EINTR);
        }

        // 第三步：检查是否已超时；若已超时则不睡眠，恢复为 runnable 并返回超时
        if let Some(deadline) = deadline {
            if is_timeout_expired(deadline) {
                drop(preempt_guard);
                restore_saved_sigmask();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }

        // 第四步：检查是否有其他未屏蔽的待处理信号打断，若被其他信号唤醒了，必须返回 EINTR 让内核去处理那个信号
        pcb.recalc_sigpending(None);
        if pcb.has_pending_signal_fast() {
            drop(preempt_guard);
            restore_saved_sigmask();
            return Err(SystemError::EINTR);
        }

        // 第五步：释放中断，然后真正进入调度睡眠（窗口期内，线程保持可中断阻塞，发送侧会唤醒）
        // 计算剩余等待时间
        let remaining_time = if let Some(deadline) = deadline {
            let now = PosixTimeSpec::now();
            let remaining = deadline.total_nanos() - now.total_nanos();
            if remaining <= 0 {
                drop(preempt_guard);
                restore_saved_sigmask();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            remaining / (NSEC_PER_JIFFY as i64)
        } else {
            i64::MAX
        };

        drop(preempt_guard);
        if let Err(e) = schedule_timeout(remaining_time) {
            restore_saved_sigmask();
            return Err(e);
        }
        // 被唤醒后回到循环起点，继续重检条件
    }
}

/// 尝试从信号队列中取出信号
fn try_dequeue_signal(awaited: &SigSet) -> Option<(Signal, SigInfo)> {
    let pcb = ProcessManager::current_pcb();
    let _current_pid = pcb.raw_pid();

    let mut siginfo_guard = pcb.sig_info_mut();
    let pending = siginfo_guard.sig_pending_mut();

    // 检查 per-thread pending
    // 仅允许从 awaited 集合中取信号：将"忽略掩码"设为 !awaited
    let ignore_mask = !*awaited;
    if let (sig, Some(info)) = pending.dequeue_signal(&ignore_mask) {
        if sig != Signal::INVALID {
            return Some((sig, info));
        }
    }

    drop(siginfo_guard);

    // 检查 shared pending
    let (sig, info) = pcb.sighand().shared_pending_dequeue(&ignore_mask);

    if sig != Signal::INVALID {
        if let Some(info) = info {
            return Some((sig, info));
        }
    }

    None
}

/// 检查是否有未屏蔽的待处理信号
fn has_pending_awaited_signal(awaited: &SigSet) -> bool {
    let pcb = ProcessManager::current_pcb();
    let _current_pid = pcb.raw_pid();
    let siginfo_guard = pcb.sig_info_irqsave();
    let pending_set = siginfo_guard.sig_pending().signal();
    drop(siginfo_guard);

    let shared_pending_set = pcb.sighand().shared_pending_signal();
    let result = pending_set.union(shared_pending_set);
    // 只看 awaited 与 pending 的交集
    let intersection = result.intersection(*awaited);
    let has = !intersection.is_empty();

    has
}

/// 计算超时截止时间
fn compute_deadline(timeout: PosixTimeSpec) -> Result<PosixTimeSpec, SystemError> {
    if timeout.tv_sec < 0 || timeout.tv_nsec < 0 || timeout.tv_nsec >= 1_000_000_000 {
        return Err(SystemError::EINVAL);
    }

    let now = PosixTimeSpec::now();
    Ok(PosixTimeSpec {
        tv_sec: now.tv_sec + timeout.tv_sec,
        tv_nsec: now.tv_nsec + timeout.tv_nsec,
    })
}

/// 检查是否超时
fn is_timeout_expired(deadline: PosixTimeSpec) -> bool {
    let now = PosixTimeSpec::now();
    now.total_nanos() >= deadline.total_nanos()
}

/// 将 PosixSigInfo 拷贝到用户空间
fn copy_posix_siginfo_to_user(
    uinfo: *mut PosixSigInfo,
    info: &SigInfo,
    from_user: bool,
) -> Result<(), SystemError> {
    if uinfo.is_null() {
        return Ok(());
    }

    let posix_siginfo = info.convert_to_posix_siginfo();
    let mut writer = UserBufferWriter::new(uinfo, size_of::<PosixSigInfo>(), from_user)?;
    writer.copy_one_to_user(&posix_siginfo, 0)?;
    Ok(())
}

impl SysRtSigtimedwaitHandle {
    #[inline(always)]
    fn uthese(args: &[usize]) -> *const SigSet {
        args[0] as *const SigSet
    }

    #[inline(always)]
    fn uinfo(args: &[usize]) -> *mut PosixSigInfo {
        args[1] as *mut PosixSigInfo
    }

    #[inline(always)]
    fn uts(args: &[usize]) -> *const PosixTimeSpec {
        args[2] as *const PosixTimeSpec
    }

    #[inline(always)]
    fn sigsetsize(args: &[usize]) -> usize {
        args[3]
    }
}

impl Syscall for SysRtSigtimedwaitHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let uthese = Self::uthese(args);
        let uinfo = Self::uinfo(args);
        let uts = Self::uts(args);
        let sigsetsize = Self::sigsetsize(args);

        vec![
            FormattedSyscallParam::new("uthese", format!("{:#x}", uthese as usize)),
            FormattedSyscallParam::new("uinfo", format!("{:#x}", uinfo as usize)),
            FormattedSyscallParam::new("uts", format!("{:#x}", uts as usize)),
            FormattedSyscallParam::new("sigsetsize", format!("{}", sigsetsize)),
        ]
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let uthese = Self::uthese(args);
        let uinfo = Self::uinfo(args);
        let uts = Self::uts(args);
        let sigsetsize = Self::sigsetsize(args);
        do_kernel_rt_sigtimedwait(uthese, uinfo, uts, sigsetsize, true)
    }
}

declare_syscall!(SYS_RT_SIGTIMEDWAIT, SysRtSigtimedwaitHandle);
