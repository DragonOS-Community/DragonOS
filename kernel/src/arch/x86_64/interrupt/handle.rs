use core::{ffi::c_void, intrinsics::likely};

use crate::{
    arch::driver::apic::{CurrentApic, LocalAPIC},
    exception::{irqdesc::irq_desc_manager, IrqNumber},
    include::bindings::bindings::{do_IRQ, pt_regs},
};

use super::TrapFrame;

#[no_mangle]
unsafe extern "C" fn x86_64_do_irq(trap_frame: &mut TrapFrame, vector: u32) {
    // swapgs

    if trap_frame.from_user() {
        x86_64::registers::segmentation::GS::swap();
    }

    // 暂时只处理33号中断，其他的中断都交给do_IRQ处理
    if vector != 33 {
        return do_IRQ(
            trap_frame as *mut TrapFrame as usize as *mut pt_regs,
            vector as u64,
        );
    }

    // 由于x86上面，虚拟中断号与物理中断号是一一对应的，所以这里直接使用vector作为中断号来查询irqdesc

    let desc = irq_desc_manager().lookup(IrqNumber::new(vector));

    if likely(desc.is_some()) {
        let desc = desc.unwrap();
        let handler = desc.handler();
        if likely(handler.is_some()) {
            handler.unwrap().handle(&desc, trap_frame);
        } else {
            CurrentApic.send_eoi();
        }
    } else {
        CurrentApic.send_eoi();
    }
}
