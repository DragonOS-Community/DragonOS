use crate::{include::bindings::bindings::{process_control_block, CLONE_SIGHAND}, kdebug, libs::{refcount::{refcount_inc, RefCount}, ffi_convert::FFIBind2Rust}, arch::x86_64::asm::current::current_pcb};

#[no_mangle]
pub extern "C" fn process_copy_sighand(clone_flags: u64, pcb: *mut process_control_block) -> i32 {
    kdebug!("process_copy_sighand");
    if(clone_flags & (CLONE_SIGHAND as u64)) != 0{
        let r = RefCount::convert_mut(unsafe{&mut (*((current_pcb().sighand))).count}).unwrap();
       refcount_inc(r);
    }
    0
}

#[no_mangle]
pub extern "C" fn process_copy_signal(clone_flags: u64, pcb: *mut process_control_block) -> i32 {
    kdebug!("process_copy_signal");
    0
}

#[no_mangle]
pub extern "C" fn process_exit_signal(pcb: *mut process_control_block){
    // todo: 回收进程的信号结构体
}

#[no_mangle]
pub extern "C" fn process_exit_sighand(pcb: *mut process_control_block){
    // todo: 回收进程的sighand结构体
}
