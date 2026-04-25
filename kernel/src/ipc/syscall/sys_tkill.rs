use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::process::pid::PidType;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{
    arch::{ipc::signal::Signal, syscall::nr::SYS_TKILL},
    ipc::signal_types::SigCode,
    process::{ProcessControlBlock, ProcessManager, RawPid},
};
use system_error::SystemError;

use crate::ipc::signal_types::{SigInfo, SigType};
use crate::process::cred::CAPFlags;

/// tkill系统调用处理器
pub struct SysTkillHandle;

impl SysTkillHandle {
    #[inline(always)]
    fn tid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        args[1] as c_int
    }
}

impl Syscall for SysTkillHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tid = Self::tid(args);
        let sig = Self::sig(args);

        // 参数验证
        if tid <= 0 {
            return Err(SystemError::EINVAL);
        }

        // 调用通用实现，tgid=0表示不验证线程组
        do_tkill(0, tid, sig)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("tid", Self::tid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
        ]
    }
}

/// 通用的线程信号发送函数
///
/// 该函数是tkill和tgkill的核心实现，通过tgid参数区分行为：
/// - 当tgid=0时，不验证线程组归属（tkill行为）
/// - 当tgid>0时，验证线程组归属（tgkill行为）
///
/// # 参数
/// - `tgid`: 线程组ID，0表示不验证线程组归属
/// - `tid`: 目标线程ID
/// - `sig`: 信号值
///
/// # 返回值
/// - `Ok(0)`: 成功
/// - `Err(SystemError::EINVAL)`: 参数非法
/// - `Err(SystemError::ESRCH)`: 目标线程不存在或线程组不匹配
/// - `Err(SystemError::EPERM)`: 权限不足
pub fn do_tkill(tgid: i32, tid: i32, sig: c_int) -> Result<usize, SystemError> {
    // 1. 查找目标线程
    let target_pcb =
        ProcessManager::find_task_by_vpid(RawPid::from(tid as usize)).ok_or(SystemError::ESRCH)?;

    // 2. 验证线程组归属 (仅当tgid > 0时)
    if tgid > 0 {
        let target_tgid = target_pcb.task_tgid_vnr().ok_or(SystemError::ESRCH)?;

        if target_tgid != RawPid::from(tgid as usize) {
            return Err(SystemError::ESRCH);
        }
    }

    // 3. 探测模式处理 (sig == 0)
    if sig == 0 {
        return Ok(0);
    }

    // 4. 信号有效性检查
    let signal = Signal::from(sig);
    if signal == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }

    // 5. 权限检查
    check_kill_permission(signal, &target_pcb)?;

    // 6. 发送信号
    send_signal_to_thread(signal, target_pcb)
}

/// 检查发送信号的权限
///
/// 根据Linux的权限检查规则：
/// 1. 发送者和接收者同用户，或者
/// 2. 发送者具有CAP_KILL权限
/// 3. 对于SIGKILL和SIGSTOP，需要更严格的权限检查
///
/// # 参数
/// - `sig`: 要发送的信号
/// - `target_pcb`: 目标进程控制块
///
/// # 返回值
/// - `Ok(())`: 权限检查通过
/// - `Err(SystemError::EPERM)`: 权限不足
fn check_kill_permission(
    sig: Signal,
    target_pcb: &Arc<ProcessControlBlock>,
) -> Result<(), SystemError> {
    let current_pcb = ProcessManager::current_pcb();
    let current_cred = current_pcb.cred();
    let target_cred = target_pcb.cred();

    // 检查是否具有CAP_KILL权限
    if current_cred.has_capability(CAPFlags::CAP_KILL) {
        return Ok(());
    }

    // 检查是否为同一用户
    if current_cred.euid == target_cred.euid {
        return Ok(());
    }

    // 对于SIGKILL和SIGSTOP，需要更严格的权限检查
    if matches!(sig, Signal::SIGKILL | Signal::SIGSTOP) {
        if current_cred.has_capability(CAPFlags::CAP_KILL) {
            return Ok(());
        }
        return Err(SystemError::EPERM);
    }

    // 其他信号，如果不同用户且没有CAP_KILL权限，则拒绝
    Err(SystemError::EPERM)
}

/// 向指定线程发送信号
///
/// # 参数
/// - `sig`: 要发送的信号
/// - `target_pcb`: 目标进程控制块
///
/// # 返回值
/// - `Ok(0)`: 成功
/// - `Err(SystemError::ESRCH)`: 目标线程在投递过程中退出（竞态容忍）
fn send_signal_to_thread(
    sig: Signal,
    target_pcb: Arc<ProcessControlBlock>,
) -> Result<usize, SystemError> {
    // 构造SigInfo，使用SI_TKILL语义
    let current_pcb = ProcessManager::current_pcb();
    let current_tgid = current_pcb.task_tgid_vnr().unwrap_or(RawPid::from(0));
    let sender_uid = current_pcb.cred().uid.data() as u32;

    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Tkill, // 使用SI_TKILL语义
        SigType::Kill {
            pid: current_tgid,
            uid: sender_uid,
        },
    );

    // 发送信号（tgkill 发送线程级信号，使用 PidType::PID）
    let result = sig.send_signal_info_to_pcb(Some(&mut info), target_pcb, PidType::PID);

    // 处理竞态条件：如果目标线程在投递过程中退出，视为成功
    match result {
        Err(SystemError::ESRCH) => Ok(0), // 竞态容忍
        other => other.map(|_| 0),
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_TKILL, SysTkillHandle);
