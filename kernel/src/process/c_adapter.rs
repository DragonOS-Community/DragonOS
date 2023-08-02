use core::{ffi::c_void, ptr::null_mut};

use alloc::boxed::Box;

use crate::{
    arch::{asm::current::current_pcb, fpu::FpState},
    include::bindings::bindings::process_control_block,
    syscall::SystemError,
};

use super::{fork::copy_mm, process::init_stdio, process_init};

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
