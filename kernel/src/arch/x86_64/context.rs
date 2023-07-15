use crate::include::bindings::bindings::{process_control_block, switch_proc};

use core::sync::atomic::compiler_fence;

use super::fpu::{fp_state_restore, fp_state_save};

/// @brief 切换进程的上下文（没有切换页表的动作）
///
/// @param next 下一个进程的pcb
/// @param trap_frame 中断上下文的栈帧
#[inline(always)]
pub fn switch_process(
    prev: &'static mut process_control_block,
    next: &'static mut process_control_block,
) {
    fp_state_save(prev);
    fp_state_restore(next);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let new_address_space = next.address_space().expect("Next process: address space is null");
    unsafe {
        // 加载页表
        new_address_space.write().user_mapper.utable.make_current();
        switch_proc(prev, next);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
