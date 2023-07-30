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
pub extern "C" fn rs_process_copy_mm(clone_vm: bool, new_pcb: &mut process_control_block) -> usize {
    return copy_mm(clone_vm, new_pcb)
        .map(|_| 0)
        .unwrap_or_else(|err| err.to_posix_errno() as usize);
}

/// @brief 初始化当前进程的文件描述符数组
/// 请注意，如果当前进程已经有文件描述符数组，那么本操作将被禁止
#[no_mangle]
pub extern "C" fn process_init_files() -> i32 {
    let r = current_pcb().init_files();
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err().to_posix_errno();
    }
}

#[no_mangle]
pub extern "C" fn rs_drop_address_space(pcb: &'static mut process_control_block) -> i32 {
    unsafe {
        pcb.drop_address_space();
    }
    return 0;
}

/// @brief 拷贝当前进程的文件描述符信息
///
/// @param clone_flags 克隆标志位
/// @param pcb 新的进程的pcb
#[no_mangle]
pub extern "C" fn process_copy_files(
    clone_flags: u64,
    from: &'static process_control_block,
) -> i32 {
    let r = current_pcb().copy_files(clone_flags, from);
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err().to_posix_errno();
    }
}

/// @brief 回收进程的文件描述符数组
///
/// @param pcb 要被回收的进程的pcb
///
/// @return i32
#[no_mangle]
pub extern "C" fn process_exit_files(pcb: &'static mut process_control_block) -> i32 {
    let r: Result<(), SystemError> = pcb.exit_files();
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err().to_posix_errno();
    }
}

/// @brief 复制当前进程的浮点状态
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn rs_dup_fpstate() -> *mut c_void {
    // 如果当前进程没有浮点状态，那么就返回一个默认的浮点状态
    if current_pcb().fp_state == null_mut() {
        return Box::leak(Box::new(FpState::default())) as *mut FpState as usize as *mut c_void;
    } else {
        // 如果当前进程有浮点状态，那么就复制一个新的浮点状态
        let state = current_pcb().fp_state as usize as *mut FpState;
        unsafe {
            let s = state.as_ref().unwrap();
            let state: &mut FpState = Box::leak(Box::new(s.clone()));

            return state as *mut FpState as usize as *mut c_void;
        }
    }
}

/// @brief 释放进程的浮点状态所占用的内存
#[no_mangle]
pub extern "C" fn rs_process_exit_fpstate(pcb: &'static mut process_control_block) {
    if pcb.fp_state != null_mut() {
        let state = pcb.fp_state as usize as *mut FpState;
        unsafe {
            drop(Box::from_raw(state));
        }
    }
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
