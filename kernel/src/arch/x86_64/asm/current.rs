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
