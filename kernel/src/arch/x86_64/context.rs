use crate::include::bindings::bindings::{__switch_to, process_control_block, pt_regs};
use crate::kdebug;

use core::arch::asm;
use core::sync::atomic::compiler_fence;

use super::mm::switch_mm;

/// @brief 切换进程的上下文（没有切换页表的动作）
///
/// @param next 下一个进程的pcb
/// @param trap_frame 中断上下文的栈帧
// #[inline(always)]
#[allow(named_asm_labels)]
pub fn switch_process(
    prev: &'static mut process_control_block,
    next: &'static mut process_control_block,
    trap_frame: &'static mut pt_regs,
) {
    // kdebug!("switch process");
    // 切换页表
    // let next = switch_mm(next);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // 更新中断栈帧
    unsafe {
        (*prev.thread).trap_frame = trap_frame.clone();
        *trap_frame = (*next.thread).trap_frame;
    }
    // kdebug!("next trap frame={:?}", trap_frame);
    // 切换fs gs
    unsafe {
        __switch_to(prev, next);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
