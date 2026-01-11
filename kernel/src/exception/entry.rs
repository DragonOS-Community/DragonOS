use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal, CurrentIrqArch, CurrentSignalArch},
    exception::InterruptArch,
    ipc::signal_types::SignalArch,
    process::{rseq::Rseq, ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
};

/// 退出到用户态之前，在这个函数内做最后的处理
///
/// # Safety
///
/// 由于此函数内可能会直接退出进程，在进入之前必须保证所有栈上的 Arc/Box 指针已被释，否则可能导致内存泄漏。
unsafe fn exit_to_user_mode_loop(frame: &mut TrapFrame) {
    loop {
        // 必须在关中断下读取标志，防止竞态
        CurrentIrqArch::interrupt_disable();

        let pcb = ProcessManager::current_pcb();
        let flags = *pcb.flags();

        // 筛选出需要处理的标志位（信号、调度、RSEQ 等）
        let work = flags.exit_to_user_mode_work();
        if work.is_empty() {
            // 无工作，保持关中断返回
            break;
        }

        // 有工作，必须开中断处理
        // 释放 PCB 引用，避免持有自旋锁或导致引用计数问题
        drop(pcb);
        CurrentIrqArch::interrupt_enable();

        // 处理调度 (Linux: _TIF_NEED_RESCHED)，无论是 syscall 还是 irq 返回，都必须检查抢占！
        if flags.contains(ProcessFlags::NEED_SCHEDULE) {
            schedule(SchedMode::SM_NONE);
        }

        // 处理信号 (Linux: _TIF_SIGPENDING)
        // Linux 通常先处理信号。如果信号导致了栈帧改变（跳去 Handler），RSEQ 的处理将推迟到 Handler 返回时。
        if flags.contains(ProcessFlags::HAS_PENDING_SIGNAL) {
            CurrentSignalArch::do_signal_or_restart(frame);
        }

        // 处理 RSEQ / Notify Resume (Linux: _TIF_NOTIFY_RESUME)
        if flags.contains(ProcessFlags::NEED_RSEQ)
            && Rseq::handle_notify_resume(Some(frame)).is_err()
        {
            let pcb = ProcessManager::current_pcb();
            let _ = crate::ipc::kill::send_signal_to_pcb(pcb, Signal::SIGSEGV);
        }

        // 循环继续，再次关中断检查是否有新工作产生
    }

    // 循环结束，所有工作已完成，保持关中断状态返回到汇编层
    // 汇编代码将执行 iret/sysret 返回用户态
}

/// 从系统调用返回到用户态的统一退出路径
/// 对应 Linux 6.6.21 arch/x86/entry/common.c::syscall_return_to_user_mode
///
/// # Safety
///
/// 由于此函数内可能会直接退出进程，在进入之前必须保证所有栈上的 Arc/Box 指针已被释放
#[no_mangle]
pub unsafe extern "C" fn syscall_exit_to_user_mode(frame: &mut TrapFrame) {
    // 这一步必须在 flags 检查之外进行，因为它是一个独立的安全检查
    Rseq::rseq_syscall_check(frame);
    // 系统调用直接调用统一循环
    exit_to_user_mode_loop(frame);
}

/// 从中断/异常返回到用户态的退出路径
/// 对应 Linux 6.6.21 kernel/entry/common.c::irqentry_exit_to_user_mode
/// 用于处理非系统调用的返回路径（如中断返回）
#[no_mangle]
pub unsafe extern "C" fn irqentry_exit(frame: &mut TrapFrame) {
    // 只有返回用户态时才处理
    if frame.is_from_user() {
        exit_to_user_mode_loop(frame);
    }
}
