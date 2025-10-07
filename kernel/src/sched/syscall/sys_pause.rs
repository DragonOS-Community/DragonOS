use alloc::vec::Vec;

use crate::arch::{interrupt::TrapFrame, CurrentIrqArch};
use crate::exception::InterruptArch;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{arch::syscall::nr::SYS_PAUSE, process::ProcessManager};
use system_error::SystemError;

/// pause系统调用处理器
pub struct SysPauseHandle;

impl Syscall for SysPauseHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // pause()系统调用：暂停当前进程直到收到信号
        // 参考Linux的pause实现：使进程进入可中断睡眠状态，直到收到信号

        let current_pcb = ProcessManager::current_pcb();

        // 检查是否已经有待处理的信号
        if current_pcb.has_pending_signal_fast() {
            // 如果已经有待处理的信号，直接返回EINTR
            return Err(SystemError::EINTR);
        }

        // 禁用中断
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 设置进程为可中断睡眠状态
        ProcessManager::mark_sleep(true)?;

        // 释放中断保护
        drop(irq_guard);

        // 调度出去，等待信号唤醒
        crate::sched::schedule(crate::sched::SchedMode::SM_NONE);

        // 如果到达这里，说明被信号唤醒，返回EINTR
        Err(SystemError::EINTR)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_PAUSE, SysPauseHandle);
