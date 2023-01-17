pub mod barrier;
use crate::include::bindings::bindings::process_control_block;

use core::arch::asm;
use core::ptr::read_volatile;

use self::barrier::mfence;

/// @brief 切换进程的页表
///
/// @param 下一个进程的pcb。将会把它的页表切换进来。
///
/// @return 下一个进程的pcb(把它return的目的主要是为了归还所有权)
#[inline(always)]
#[allow(dead_code)]
pub fn switch_mm(
    next_pcb: &'static mut process_control_block,
) -> &'static mut process_control_block {
    mfence();
    // kdebug!("to get pml4t");
    let pml4t = unsafe { read_volatile(&next_pcb.mm.as_ref().unwrap().pgd) };

    unsafe {
        asm!("mov cr3, {}", in(reg) pml4t);
    }
    mfence();
    return next_pcb;
}
