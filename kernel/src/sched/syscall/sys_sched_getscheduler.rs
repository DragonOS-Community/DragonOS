use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_GETSCHEDULER;
use crate::process::ProcessManager;
use crate::process::RawPid;
use crate::sched::SchedPolicy;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::vec::Vec;

/// Linux 调度策略枚举
/// 与 musl-libc 和 Linux 内核保持一致
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PosixLinuxSchedPolicy {
    /// 普通调度策略（对应 CFS）
    Other = 0,
    /// 先进先出实时调度
    Fifo = 1,
    /// 轮转实时调度
    Rr = 2,
    /// 批处理调度（DragonOS 暂不支持）
    #[allow(dead_code)]
    Batch = 3,
    /// IDLE 调度
    Idle = 5,
    /// 截止时间调度（DragonOS 暂不支持）
    #[allow(dead_code)]
    Deadline = 6,
}

/// System call handler for the `sched_getscheduler` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for getting
/// the scheduling policy of a process.
struct SysSchedGetscheduler;

impl Syscall for SysSchedGetscheduler {
    /// Returns the number of arguments expected by the `sched_getscheduler` syscall
    fn num_args(&self) -> usize {
        1
    }

    /// Handles the `sched_getscheduler` system call
    ///
    /// Gets the scheduling policy of the specified process.
    /// If pid is 0, gets the scheduling policy of the current process.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Process ID (pid_t), 0 for current process
    /// * `_frame` - Trap frame (unused in this implementation)
    ///
    /// # Returns
    /// * `Ok(policy)`: Success, returns the scheduling policy value
    /// * `Err(SystemError::ESRCH)`: Process not found
    /// * `Err(SystemError::EPERM)`: Permission denied
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);

        // 获取目标进程
        let target_pcb = if pid == 0 {
            // pid 为 0 表示当前进程
            ProcessManager::current_pcb()
        } else {
            // 查找指定进程
            let raw_pid = RawPid::from(pid);
            ProcessManager::find_task_by_vpid(raw_pid).ok_or(SystemError::ESRCH)?
        };

        // 权限检查：只有进程自己或具有 CAP_SYS_NICE 权限的进程可以查询
        let current_pcb = ProcessManager::current_pcb();
        if !super::util::has_sched_permission(&current_pcb, &target_pcb) {
            return Err(SystemError::EPERM);
        }

        // 获取调度策略
        let policy = *target_pcb.sched_info().sched_policy.read_irqsave();

        // 将 DragonOS 的 SchedPolicy 映射到 Linux 的调度策略值
        // Linux 调度策略值：
        // - SCHED_OTHER = 0 (对应 CFS)
        // - SCHED_FIFO = 1
        // - SCHED_RR = 2 (实时轮转调度)
        // - SCHED_BATCH = 3 (DragonOS 暂不支持)
        // - SCHED_IDLE = 5
        // - SCHED_DEADLINE = 6 (DragonOS 暂不支持)
        let linux_policy = match policy {
            SchedPolicy::CFS => PosixLinuxSchedPolicy::Other,
            SchedPolicy::FIFO => PosixLinuxSchedPolicy::Fifo,
            SchedPolicy::RT => PosixLinuxSchedPolicy::Rr, // RT 策略映射到 SCHED_RR
            SchedPolicy::IDLE => PosixLinuxSchedPolicy::Idle,
        };

        Ok(linux_policy as i32 as usize)
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - The raw syscall arguments
    ///
    /// # Returns
    /// Vector of formatted parameters with descriptive names
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "pid",
            Self::pid(args).to_string(),
        )]
    }
}

impl SysSchedGetscheduler {
    /// Extracts the process ID from syscall arguments
    fn pid(args: &[usize]) -> usize {
        args[0]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_GETSCHEDULER, SysSchedGetscheduler);
