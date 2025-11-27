use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_GETPARAM;
use crate::process::ProcessManager;
use crate::process::RawPid;
use crate::sched::prio::PrioUtil;
use crate::sched::prio::MAX_RT_PRIO;
use crate::sched::SchedPolicy;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::string::ToString;
use alloc::vec::Vec;

/// Linux sched_param 结构体
/// 与 musl-libc 中的定义保持一致
#[repr(C)]
#[derive(Clone, Copy)]
struct PosixSchedParam {
    sched_priority: i32,
    __reserved1: i32,
    __reserved2: [i64; 4],
    __reserved3: i32,
}

/// System call handler for the `sched_getparam` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for getting
/// scheduling parameters of a process.
struct SysSchedGetparam;

impl Syscall for SysSchedGetparam {
    /// Returns the number of arguments expected by the `sched_getparam` syscall
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the `sched_getparam` system call
    ///
    /// Gets the scheduling parameters of the specified process.
    /// If pid is 0, gets the scheduling parameters of the current process.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: Process ID (pid_t), 0 for current process
    ///   - args[1]: Pointer to sched_param structure (*mut SchedParam)
    /// * `frame` - Trap frame, used to determine if call originates from user space
    ///
    /// # Returns
    /// * `Ok(0)`: Success
    /// * `Err(SystemError::ESRCH)`: Process not found
    /// * `Err(SystemError::EFAULT)`: Invalid user space pointer
    /// * `Err(SystemError::EPERM)`: Permission denied
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let param = Self::param(args);

        // 验证用户空间指针
        if param.is_null() {
            return Err(SystemError::EFAULT);
        }

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

        // 获取调度策略和优先级
        let policy = *target_pcb.sched_info().sched_policy.read_irqsave();
        let prio_data = target_pcb.sched_info().prio_data.read_irqsave();
        let prio = prio_data.prio;

        // 根据调度策略计算 sched_priority
        // Linux 行为：
        // - 对于普通进程（SCHED_OTHER/CFS/IDLE），sched_priority 始终为 0
        // - 对于实时进程（SCHED_FIFO/SCHED_RR），sched_priority 范围是 1-99
        //   其中 1 是最低优先级，99 是最高优先级
        // - 内部优先级 prio 范围是 0-99（对于实时进程），其中 0 是最高优先级
        // - 转换公式：sched_priority = MAX_RT_PRIO (100) - prio
        //   但需要限制在 1-99 范围内（因为 prio=0 时 sched_priority=100，需要限制为 99）
        let sched_priority = match policy {
            SchedPolicy::CFS | SchedPolicy::IDLE => {
                // 普通进程的 sched_priority 始终为 0
                0
            }
            SchedPolicy::RT | SchedPolicy::FIFO => {
                // 检查是否为有效的实时优先级
                // 实时进程的 prio 应该在 0-99 范围内（prio < MAX_RT_PRIO）
                if !PrioUtil::rt_prio(prio) {
                    // 如果优先级不在实时范围内，返回 0（表示普通进程）
                    // 这通常不应该发生，但为了健壮性，我们处理这种情况
                    0
                } else {
                    // 实时优先级转换：sched_priority = MAX_RT_PRIO - prio
                    // prio = 0（最高）→ sched_priority = 100，限制为 99
                    // prio = 99（最低）→ sched_priority = 1
                    let rt_prio = MAX_RT_PRIO - prio;
                    // 确保结果在 1-99 范围内
                    rt_prio.clamp(1, 99)
                }
            }
        };

        // 构造 sched_param 结构
        let sched_param = PosixSchedParam {
            sched_priority,
            __reserved1: 0,
            __reserved2: [0; 4],
            __reserved3: 0,
        };

        // 将结果写入用户空间
        let mut writer = UserBufferWriter::new(
            param,
            core::mem::size_of::<PosixSchedParam>(),
            frame.is_from_user(),
        )?;

        writer.buffer_protected(0)?.write_one(0, &sched_param)?;

        Ok(0)
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
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("param", format!("{:#x}", Self::param(args) as usize)),
        ]
    }
}

impl SysSchedGetparam {
    /// Extracts the process ID from syscall arguments
    fn pid(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the sched_param pointer from syscall arguments
    fn param(args: &[usize]) -> *mut PosixSchedParam {
        args[1] as *mut PosixSchedParam
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_GETPARAM, SysSchedGetparam);
