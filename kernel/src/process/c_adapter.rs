use crate::driver::uart::uart::{c_uart_send_str, UartDriver};

use super::{kthread::kthread_init, process::stdio_init, process_init, ProcessManager};

#[no_mangle]
pub extern "C" fn rs_process_init() {
    process_init();
}

#[no_mangle]
pub extern "C" fn rs_kthread_init() {
    kthread_init();
}

/// 临时用于获取空闲进程的栈顶的函数，这个函数是为了旧的smp模块的初始化而写在这的
#[no_mangle]
pub extern "C" fn rs_get_idle_stack_top(cpu_id: u32) -> usize {
    return ProcessManager::idle_pcb()[cpu_id as usize]
        .kernel_stack()
        .stack_max_address()
        .data();
}

#[no_mangle]
pub extern "C" fn rs_current_pcb_cpuid() -> u32 {
    return ProcessManager::current_pcb()
        .sched_info()
        .on_cpu()
        .unwrap_or(u32::MAX);
}
#[no_mangle]
pub extern "C" fn rs_current_pcb_pid() -> u32 {
    return ProcessManager::current_pcb().pid().0 as u32;
}

#[no_mangle]
pub extern "C" fn rs_current_pcb_preempt_count() -> u32 {
    return ProcessManager::current_pcb().preempt_count() as u32;
}

#[no_mangle]
pub extern "C" fn rs_current_pcb_flags() -> u32 {
    return ProcessManager::current_pcb().flags().bits() as u32;
}

#[no_mangle]
pub extern "C" fn rs_current_pcb_thread_rbp() -> u64 {
    return ProcessManager::current_pcb().arch_info_irqsave().rbp() as u64;
}

#[no_mangle]
pub extern "C" fn rs_preempt_disable() {
    return ProcessManager::preempt_disable();
}

#[no_mangle]
pub extern "C" fn rs_preempt_enable() {
    return ProcessManager::preempt_enable();
}

#[no_mangle]
pub extern "C" fn rs_process_do_exit(exit_code: usize) -> usize {
    ProcessManager::exit(exit_code);
}
