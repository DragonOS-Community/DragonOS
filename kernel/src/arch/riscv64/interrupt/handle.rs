//! 处理中断和异常
//!
//! 架构相关的处理逻辑参考： https://code.dragonos.org.cn/xref/linux-6.6.21/arch/riscv/kernel/traps.c
use core::hint::spin_loop;

use log::{error, trace};
use system_error::SystemError;

use super::TrapFrame;
use crate::exception::{ebreak::EBreak, extable::ExceptionTableManager};
use crate::{
    arch::{ipc::signal::Signal, syscall::syscall_handler, CurrentIrqArch, MMArch},
    driver::{clocksource::timer_riscv::RiscVSbiTimer, irqchip::riscv_intc::riscv_intc_irq},
    exception::{softirq::do_softirq, InterruptArch},
    ipc::{
        signal::{force_kernel_signal_to_current, force_sig_fault_to_current},
        signal_types::{SEGV_ACCERR, SEGV_MAPERR},
    },
    mm::{ucontext::AddressSpace, VirtAddr},
    process::{utils::current_pcb_flags, ProcessFlags, ProcessManager},
    sched::{SchedMode, __schedule},
};

type ExceptionHandler = fn(&mut TrapFrame) -> Result<(), SystemError>;

static EXCEPTION_HANDLERS: [ExceptionHandler; 16] = [
    do_trap_insn_misaligned,    // 0
    do_trap_insn_access_fault,  // 1
    do_trap_insn_illegal,       // 2
    do_trap_break,              // 3
    do_trap_load_misaligned,    // 4
    do_trap_load_access_fault,  // 5
    do_trap_store_misaligned,   // 6
    do_trap_store_access_fault, // 7
    do_trap_user_env_call,      // 8
    default_handler,            // 9
    default_handler,            // 10
    default_handler,            // 11
    do_trap_insn_page_fault,    // 12
    do_trap_load_page_fault,    // 13
    default_handler,            // 14
    do_trap_store_page_fault,   // 15
];

const ILL_ILLOPC: i32 = 1;
const ILL_ILLTRP: i32 = 4;
const BUS_ADRALN: i32 = 1;
const TRAP_BRKPT: i32 = 1;

fn user_trap_signal(signal: Signal, code: i32, address: usize) -> Result<(), SystemError> {
    unsafe { CurrentIrqArch::interrupt_enable() };
    let result = force_sig_fault_to_current(signal, code, VirtAddr::new(address));
    unsafe { CurrentIrqArch::interrupt_disable() };
    result
}

fn user_page_fault_signal(address: usize) -> Result<(), SystemError> {
    unsafe { CurrentIrqArch::interrupt_enable() };
    let address = VirtAddr::new(address);
    let code = if address.check_user()
        && AddressSpace::current().is_ok_and(|mm| mm.read().mappings.contains(address).is_some())
    {
        SEGV_ACCERR
    } else {
        SEGV_MAPERR
    };
    let result = force_sig_fault_to_current(Signal::SIGSEGV, code, address);
    unsafe { CurrentIrqArch::interrupt_disable() };
    result
}

fn try_fixup_kernel_user_access(trap_frame: &mut TrapFrame) -> bool {
    if trap_frame.is_from_user() || ProcessManager::current_pcb().pagefault_disabled() == 0 {
        return false;
    }
    if !VirtAddr::new(trap_frame.badaddr).check_user() {
        return false;
    }

    if let Some(fixup_addr) = ExceptionTableManager::search_exception_table(trap_frame.epc) {
        trap_frame.epc = fixup_addr;
        return true;
    }

    false
}

#[no_mangle]
unsafe extern "C" fn riscv64_do_irq(trap_frame: &mut TrapFrame) {
    let from_user = trap_frame.is_from_user();
    if from_user {
        crate::rcu::user_exit();
    }

    if trap_frame.cause.is_interrupt() {
        let hardirq_guard = crate::exception::enter_hardirq();
        let rcu_irq = crate::rcu::irq_enter();
        riscv64_do_interrupt(trap_frame);
        let irq_outermost = rcu_irq.is_outermost();
        if irq_outermost {
            do_softirq();
        }

        let should_schedule = current_pcb_flags().contains(ProcessFlags::NEED_SCHEDULE)
            || trap_frame.cause.code() as u32 == RiscVSbiTimer::TIMER_IRQ.data();
        let disposition = if should_schedule && irq_outermost {
            crate::rcu::RcuIrqDisposition::ToKernel
        } else {
            crate::rcu::RcuIrqDisposition::ResumeInterrupted
        };
        crate::rcu::irq_exit(rcu_irq, disposition);
        drop(hardirq_guard);

        if should_schedule && irq_outermost {
            __schedule(SchedMode::SM_PREEMPT);
        }
    } else if trap_frame.cause.is_exception() {
        riscv64_do_exception(trap_frame);
    }
}

/// 处理中断
fn riscv64_do_interrupt(trap_frame: &mut TrapFrame) {
    riscv_intc_irq(trap_frame);
}

/// 处理异常
fn riscv64_do_exception(trap_frame: &mut TrapFrame) {
    let code = trap_frame.cause.code();

    if code < EXCEPTION_HANDLERS.len() {
        let handler = EXCEPTION_HANDLERS[code];
        handler(trap_frame).ok();
    } else {
        if trap_frame.is_from_user() {
            default_handler(trap_frame).ok();
            return;
        }
        error!("riscv64_do_irq: exception code out of range");
        loop {
            // kernel die
            spin_loop();
        }
    };
}

fn default_handler(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGILL, ILL_ILLTRP, trap_frame.epc);
    }

    error!("riscv64_do_irq: handler not found");
    loop {
        spin_loop();
    }
}

/// 处理指令地址不对齐异常 #0
fn do_trap_insn_misaligned(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGBUS, BUS_ADRALN, trap_frame.epc);
    }

    error!("riscv64_do_irq: do_trap_insn_misaligned");
    loop {
        spin_loop();
    }
}

/// 处理指令访问异常 #1
fn do_trap_insn_access_fault(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGSEGV, SEGV_ACCERR, trap_frame.epc);
    }

    error!("riscv64_do_irq: do_trap_insn_access_fault");
    loop {
        spin_loop();
    }
}

/// 处理非法指令异常 #2
fn do_trap_insn_illegal(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGILL, ILL_ILLOPC, trap_frame.epc);
    }

    error!("riscv64_do_irq: do_trap_insn_illegal");
    loop {
        spin_loop();
    }
}

/// 处理断点异常 #3
fn do_trap_break(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGTRAP, TRAP_BRKPT, trap_frame.epc);
    }

    trace!("riscv64_do_irq: do_trap_break");
    // handle breakpoint
    EBreak::handle(trap_frame)
}

/// 处理加载地址不对齐异常 #4
fn do_trap_load_misaligned(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGBUS, BUS_ADRALN, trap_frame.epc);
    }

    error!("riscv64_do_irq: do_trap_load_misaligned");
    loop {
        spin_loop();
    }
}

/// 处理加载访问异常 #5
fn do_trap_load_access_fault(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGSEGV, SEGV_ACCERR, trap_frame.epc);
    }

    if try_fixup_kernel_user_access(trap_frame) {
        return Ok(());
    }

    error!("riscv64_do_irq: do_trap_load_access_fault");
    loop {
        spin_loop();
    }
}

/// 处理存储地址不对齐异常 #6
fn do_trap_store_misaligned(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGBUS, BUS_ADRALN, trap_frame.epc);
    }

    error!("riscv64_do_irq: do_trap_store_misaligned");
    loop {
        spin_loop();
    }
}

/// 处理存储访问异常 #7
fn do_trap_store_access_fault(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        return user_trap_signal(Signal::SIGSEGV, SEGV_ACCERR, trap_frame.epc);
    }

    if try_fixup_kernel_user_access(trap_frame) {
        return Ok(());
    }

    error!("riscv64_do_irq: do_trap_store_access_fault");
    loop {
        spin_loop();
    }
}

/// 处理环境调用异常 #8
fn do_trap_user_env_call(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        let syscall_num = trap_frame.a7;
        trap_frame.epc += 4;
        trap_frame.origin_a0 = trap_frame.a0;
        syscall_handler(syscall_num, trap_frame);
    } else {
        panic!("do_trap_user_env_call: not from user mode")
    }
    Ok(())
}

// 9-11 reserved

/// 处理指令页错误异常 #12
fn do_trap_insn_page_fault(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    let vaddr = trap_frame.badaddr;
    let cause = trap_frame.cause;
    let epc = trap_frame.epc;
    if trap_frame.is_from_user() {
        return user_page_fault_signal(vaddr);
    } else {
        panic!(
            "riscv64_do_irq: do_trap_insn_page_fault(kernel mode): epc: {epc:#x}, vaddr={:#x}, cause={:?}",
            vaddr, cause
        );
    }
}

/// 处理页加载错误异常 #13
fn do_trap_load_page_fault(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    let vaddr = trap_frame.badaddr;
    let cause = trap_frame.cause;
    let epc = trap_frame.epc;
    if trap_frame.is_from_user() {
        return user_page_fault_signal(vaddr);
    } else if try_fixup_kernel_user_access(trap_frame) {
        return Ok(());
    } else {
        panic!(
            "riscv64_do_irq: do_trap_load_page_fault(kernel mode): epc: {epc:#x}, vaddr={:#x}, cause={:?}",
            vaddr, cause
        );
    }
}

// 14 reserved

/// 处理页存储错误异常 #15
fn do_trap_store_page_fault(trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    if trap_frame.is_from_user() {
        unsafe { CurrentIrqArch::interrupt_enable() };
        let result = MMArch::handle_user_store_page_fault(VirtAddr::new(trap_frame.badaddr));
        // A pending signal can interrupt fault retry; leave delivery to the user return path.
        if matches!(
            result,
            Ok(()) | Err(SystemError::EINTR | SystemError::ERESTARTSYS)
        ) {
            unsafe { CurrentIrqArch::interrupt_disable() };
            return Ok(());
        }
        if result == Err(SystemError::ENOMEM) {
            let result = force_kernel_signal_to_current(Signal::SIGKILL);
            unsafe { CurrentIrqArch::interrupt_disable() };
            return result;
        }
        unsafe { CurrentIrqArch::interrupt_disable() };
        return user_page_fault_signal(trap_frame.badaddr);
    } else if try_fixup_kernel_user_access(trap_frame) {
        return Ok(());
    }

    error!(
        "riscv64_do_irq: do_trap_store_page_fault: epc: {:#x}, vaddr={:#x}, cause={:?}",
        trap_frame.epc, trap_frame.badaddr, trap_frame.cause
    );
    loop {
        spin_loop();
    }
}
