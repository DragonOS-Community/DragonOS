use super::{process::init_stdio, process_init, ProcessManager};

#[no_mangle]
pub extern "C" fn rs_process_init() {
    process_init();
}

#[no_mangle]
pub extern "C" fn rs_init_stdio() -> i32 {
    let r = init_stdio();
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err().to_posix_errno();
    }
}

/// 临时用于获取空闲进程的栈顶的函数，这个函数是为了旧的smp模块的初始化而写在这的
#[no_mangle]
pub extern "C" fn rs_get_idle_stack_top(cpu_id: u32) -> usize {
    return ProcessManager::idle_pcb()[cpu_id as usize]
        .kernel_stack()
        .stack_max_address()
        .data();
}

//=======以下为对C的接口========

#[no_mangle]
pub extern "C" fn rs_current_pcb_cpuid() -> u32 {
    return ProcessManager::current_pcb()
        .sched_info()
        .on_cpu()
        .unwrap_or(u32::MAX);
}
#[no_mangle]
pub extern "C" fn rs_current_pcb_pid() -> u32 {
    return ProcessManager::current_pcb().basic().pid().0 as u32;
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
    return ProcessManager::current_pcb().arch_info().rbp() as u64;
}
