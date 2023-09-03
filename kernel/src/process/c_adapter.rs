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
pub extern "C" fn rs_current_pcb_state() -> u32 {
    return ProcessManager::current_pcb().sched_info().state();
}

#[no_mangle]
pub extern "C" fn rs_current_pcb_set_cpuid(on_cpu: u32) {
    ProcessManager::current_pcb()
        .sched_info()
        .set_on_cpu(Some(on_cpu));
}
#[no_mangle]
pub extern "C" fn rs_current_pcb_cpuid() -> u32 {
    return ProcessManager::current_pcb().sched_info().on_cpu();
}
#[no_mangle]
pub extern "C" fn rs_current_pcb_pid() -> i32 {
    return ProcessManager::current_pcb().basic().pid();
}

#[no_mangle]
pub extern "C" fn rs_current_pcb_preempt_count() -> u32 {
    return ProcessManager::current_pcb().preempt_count();
}

#[no_mangle]
pub extern "C" fn rs_current_pcb_flags() -> u32 {
    return ProcessManager::current_pcb().flags();
}
#[no_mangle]
pub extern "C" fn rs_current_pcb_set_flags(new_flags: u32) {
    ProcessManager::current_pcb().set_flags(new_flags);
}
#[no_mangle]
pub extern "C" fn rs_current_pcb_virtual_runtime() -> i32 {
    return ProcessManager::current_pcb().sched_info().virtual_runtime();
}
#[no_mangle]
pub extern "C" fn rs_current_pcb_thread_rbp() -> i64 {
    return ProcessManager::current_pcb().arch_info().get_rbp();
}

#[no_mangle]
pub extern "C" fn rs_get_current_pcb() -> *mut libc::c_void {
    let pcb_ptr = Box::into_raw(Box::new(ProcessManager::current_pcb())) as *mut libc::c_void;
    return pcb_ptr;
}
