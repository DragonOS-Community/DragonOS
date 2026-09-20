use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch, CurrentSignalArch},
    exception::InterruptArch,
    ipc::signal_types::SignalArch,
    process::{rseq::Rseq, ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
};

/// Establishes RCU's persistent kernel context before any high-level handler
/// can use ordinary RCU. Architecture entry code calls this with interrupts
/// disabled and a complete trap frame.
#[no_mangle]
unsafe extern "C" fn irqentry_enter(frame: &mut TrapFrame) {
    if frame.is_from_user() {
        crate::rcu::user_exit();
    }
}

#[no_mangle]
unsafe extern "C" fn irqentry_exit(frame: &mut TrapFrame) {
    if frame.is_from_user() {
        irqentry_exit_to_user_mode(frame);
    }
}

/// 退出到用户态之前，在这个函数内做最后的处理
///
/// # Safety
///
/// 调用者必须关闭中断；返回时仍保持中断关闭，直到架构代码返回用户态。
///
/// 由于这个函数内可能会直接退出进程，因此，在进入函数之前，
/// 必须保证所有的栈上的Arc/Box指针等，都已经被释放。否则，可能会导致内存泄漏。
unsafe fn irqentry_exit_to_user_mode(frame: &mut TrapFrame) {
    exit_to_user_mode_prepare(frame);
    #[cfg(target_arch = "x86_64")]
    crate::arch::process::table::TSSManager::update_io_bitmap_from_current();
    debug_assert!(!CurrentIrqArch::is_irq_enabled());
    crate::rcu::user_enter();
}

/// # Safety
///
/// 由于这个函数内可能会直接退出进程，因此，在进入函数之前，
/// 必须保证所有的栈上的Arc/Box指针等，都已经被释放。否则，可能会导致内存泄漏。
unsafe fn exit_to_user_mode_prepare(frame: &mut TrapFrame) {
    debug_assert!(!CurrentIrqArch::is_irq_enabled());
    let process_flags_work = ProcessManager::current_pcb().flags().load();
    if !process_flags_work.exit_to_user_mode_work().is_empty() {
        exit_to_user_mode_loop(frame, process_flags_work);
    }
}

/// # Safety
///
/// 由于这个函数内可能会直接退出进程，因此，在进入函数之前，
/// 必须保证所有的栈上的Arc/Box指针等，都已经被释放。否则，可能会导致内存泄漏。
unsafe fn exit_to_user_mode_loop(frame: &mut TrapFrame, mut process_flags_work: ProcessFlags) {
    while !process_flags_work.exit_to_user_mode_work().is_empty() {
        if process_flags_work.contains(ProcessFlags::NEED_SCHEDULE) {
            schedule(SchedMode::SM_PREEMPT);
        } else {
            // 优先处理 rseq，因为信号递送会保存 trapframe 到 sigframe
            // rseq 的 IP fixup 必须在信号递送之前完成
            if process_flags_work.contains(ProcessFlags::NEED_RSEQ) {
                let _ = Rseq::handle_notify_resume(Some(frame));
                process_flags_work = ProcessManager::current_pcb().flags().load();
            }

            // Check for a signal or a ptrace trap
            if process_flags_work.contains(ProcessFlags::HAS_PENDING_SIGNAL)
                || process_flags_work.contains(ProcessFlags::PENDING_PTRACE_STOP)
                || process_flags_work.contains(ProcessFlags::PENDING_DEBUG)
            {
                unsafe { CurrentSignalArch::do_signal_or_restart(frame) };
            }
        }

        // Signal handling may enable interrupts. Close that window before
        // rechecking work and keep IRQs disabled through RCU's user transition.
        // Do not use an irqsave guard: the return boundary must not restore IF.
        CurrentIrqArch::interrupt_disable();
        process_flags_work = ProcessManager::current_pcb().flags().load();
    }
}
