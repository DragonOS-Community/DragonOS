use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal, syscall::nr::SYS_TKILL},
    ipc::signal_types::{OriginCode, SigCode, SigInfo, SigType},
    process::{cred::CAPFlags, pid::PidType, ProcessControlBlock, ProcessManager, RawPid},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use system_error::SystemError;

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

        // 调用通用实现，tgid=0表示不验证线程组 (tkill行为)
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

    // 2. 验证线程组归属 (tgkill 逻辑)
    if tgid > 0 {
        let target_tgid = target_pcb.task_tgid_vnr().ok_or(SystemError::ESRCH)?;
        if target_tgid != RawPid::from(tgid as usize) {
            return Err(SystemError::ESRCH);
        }
    }

    // 3. 信号预处理
    // sig=0 用于探测，不产生实际 Signal 对象
    let signal = if sig == 0 {
        Signal::INVALID
    } else {
        let s = Signal::from(sig);
        if s == Signal::INVALID {
            return Err(SystemError::EINVAL);
        }
        s
    };

    // 4. 权限检查 (sig=0 时也必须检查权限)
    check_kill_permission(signal, &target_pcb)?;

    // 5. 如果是探测模式，权限检查通过后直接返回
    if sig == 0 {
        return Ok(0);
    }

    // 6. 发送信号
    send_signal_tkill(signal, target_pcb)
}

/// 检查发送信号的权限
///
/// 根据Linux的权限检查规则：
/// 1. 发送者和接收者同用户，或者发送者具有 CAP_KILL 权限
/// 2. 对于 SIGCONT，规则更宽松：只要是同一 session 即可
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

    // 检查 CAP_KILL 权限
    if current_cred.has_capability(CAPFlags::CAP_KILL) {
        return Ok(());
    }

    // 凭证检查 (kill_ok_by_cred)
    // 规则：发送者的 euid/uid 必须匹配目标线程的 suid/uid
    if current_cred.euid == target_cred.suid
        || current_cred.euid == target_cred.uid
        || current_cred.uid == target_cred.suid
        || current_cred.uid == target_cred.uid
    {
        return Ok(());
    }

    // 3. SIGCONT 的特殊规则：同一 Session 即可
    if sig == Signal::SIGCONT {
        let current_session = current_pcb.task_session();
        let target_session = target_pcb.task_session();
        // 确保双方都在 session 中且 session ID 相同
        if current_session.is_some() && current_session == target_session {
            return Ok(());
        }
    }

    Err(SystemError::EPERM)
}

/// 发送 tkill 语义的信号
fn send_signal_tkill(
    sig: Signal,
    target_pcb: Arc<ProcessControlBlock>,
) -> Result<usize, SystemError> {
    let current_pcb = ProcessManager::current_pcb();
    let current_tgid = current_pcb.task_tgid_vnr().unwrap_or(RawPid::from(0));
    let sender_uid = current_pcb.cred().uid.data() as u32;

    let mut info = SigInfo::new(
        sig,
        0, // errno
        SigCode::Origin(OriginCode::Tkill),
        SigType::Kill {
            pid: current_tgid, // 发送者的 TGID
            uid: sender_uid,
        },
    );

    match sig.send_signal_info_to_pcb(Some(&mut info), target_pcb, PidType::PID) {
        // 如果目标线程在投递过程中退出，Linux 视为成功（竞态容忍）
        Err(SystemError::ESRCH) => Ok(0),
        result => result.map(|_| 0),
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_TKILL, SysTkillHandle);
