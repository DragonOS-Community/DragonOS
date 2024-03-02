use core::{ffi::c_void, intrinsics::likely};

use crate::{
    arch::{
        driver::apic::{apic_timer::APIC_TIMER_IRQ_NUM, CurrentApic, LocalAPIC},
        sched::sched,
        CurrentIrqArch,
    },
    exception::{irqdesc::irq_desc_manager, InterruptArch, IrqNumber},
    include::bindings::bindings::{do_IRQ, pt_regs},
    kdebug,
    process::{
        process::{current_pcb_flags, current_pcb_preempt_count},
        ProcessFlags,
    },
};

use super::TrapFrame;

#[no_mangle]
unsafe extern "C" fn x86_64_do_irq(trap_frame: &mut TrapFrame, vector: u32) {
    // swapgs

    if trap_frame.from_user() {
        x86_64::registers::segmentation::GS::swap();
    }

    // 暂时只处理33号中断，其他的中断都交给do_IRQ处理
    if vector != 33
        && vector != 56
        && vector != 34
        && vector != 151
        && vector != 44
        && vector != 200
        && vector != 201
    {
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
    if current_pcb_preempt_count() > 0 {
        return;
    }
    // 检测当前进程是否可被调度
    if (current_pcb_flags().contains(ProcessFlags::NEED_SCHEDULE))
        && vector == APIC_TIMER_IRQ_NUM.data()
    {
        sched();
    }
}
