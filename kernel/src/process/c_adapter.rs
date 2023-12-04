use crate::smp::core::smp_get_processor_id;

use super::{kthread::kthread_init, process_init, ProcessManager, __PROCESS_MANAGEMENT_INIT_DONE};

#[no_mangle]
unsafe extern "C" fn rs_process_init() {
    process_init();
}

#[no_mangle]
unsafe extern "C" fn rs_kthread_init() {
    kthread_init();
}

/// 临时用于获取空闲进程的栈顶的函数，这个函数是为了旧的smp模块的初始化而写在这的
#[no_mangle]
unsafe extern "C" fn rs_get_idle_stack_top(cpu_id: u32) -> usize {
    return ProcessManager::idle_pcb()[cpu_id as usize]
        .kernel_stack()
        .stack_max_address()
        .data();
}

#[no_mangle]
unsafe extern "C" fn rs_current_pcb_cpuid() -> u32 {
    return smp_get_processor_id();
}

#[no_mangle]
unsafe extern "C" fn rs_current_pcb_pid() -> u32 {
    if unsafe { __PROCESS_MANAGEMENT_INIT_DONE } {
        return ProcessManager::current_pcb().pid().0 as u32;
    }
    return 0;
}

#[no_mangle]
unsafe extern "C" fn rs_current_pcb_preempt_count() -> u32 {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return 0;
    }
    return ProcessManager::current_pcb().preempt_count() as u32;
}

#[no_mangle]
unsafe extern "C" fn rs_current_pcb_flags() -> u32 {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return 0;
    }
    return ProcessManager::current_pcb().flags().bits() as u32;
}

#[no_mangle]
#[cfg(target_arch = "x86_64")]
unsafe extern "C" fn rs_current_pcb_thread_rbp() -> u64 {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return 0;
    }
    return ProcessManager::current_pcb().arch_info_irqsave().rbp() as u64;
}

#[no_mangle]
unsafe extern "C" fn rs_preempt_disable() {
    return ProcessManager::preempt_disable();
}

#[no_mangle]
unsafe extern "C" fn rs_preempt_enable() {
    return ProcessManager::preempt_enable();
}

#[no_mangle]
unsafe extern "C" fn rs_process_do_exit(exit_code: usize) -> usize {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return 0;
    }
    ProcessManager::exit(exit_code);
}
