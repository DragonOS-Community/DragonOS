use core::intrinsics::likely;

use crate::{
    arch::{
        driver::apic::{apic_timer::APIC_TIMER_IRQ_NUM, CurrentApic, LocalAPIC},
        sched::sched,
    },
    exception::{irqdesc::irq_desc_manager, softirq::do_softirq, IrqNumber},
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

    do_softirq();

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
