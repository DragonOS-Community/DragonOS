use crate::include::bindings::bindings::process_control_block;

use core::{arch::asm, sync::atomic::compiler_fence};

/// @brief 获取指向当前进程的pcb的可变引用
#[inline]
pub fn current_pcb() -> &'static mut process_control_block {
    let ret: Option<&mut process_control_block>;

    unsafe {
        let mut tmp: u64 = !(32767u64);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        asm!("and {0}, rsp", inout(reg)(tmp),);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        ret = (tmp as *mut process_control_block).as_mut();
    }

    ret.unwrap()
}

//=======以下为对C的接口========
//C语言中还有使用current_pcb->thread->rbp

#[no_mangle]
pub extern "C" fn current_pcb_state() -> u64 {
    return current_pcb().state;
}
#[no_mangle]
pub extern "C" fn current_pcb_cpu_id() -> u32 {
    return current_pcb().cpu_id;
}
#[no_mangle]
pub extern "C" fn current_pcb_pid() -> i64 {
    return current_pcb().pid;
}
#[no_mangle]
pub extern "C" fn current_pcb_preempt_count() -> i32 {
    return current_pcb().preempt_count;
}
#[no_mangle]
pub extern "C" fn current_pcb_flags() -> u64 {
    return current_pcb().flags;
}
#[no_mangle]
pub extern "C" fn current_pcb_virtual_runtime() -> i64{
    return current_pcb().virtual_runtime;
}