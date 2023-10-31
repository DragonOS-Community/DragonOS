use super::{new_timer::LocalApicTimerIntrController, CurrentApic, LocalAPIC};

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_install(irq_num: u8) {
    LocalApicTimerIntrController.install(irq_num);
}

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_uninstall(irq_num: u8) {
    LocalApicTimerIntrController.uninstall();
}

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_enable(irq_num: u8) {
    LocalApicTimerIntrController.enable();
}

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_disable(irq_num: u8) {
    LocalApicTimerIntrController.disable();
}

#[no_mangle]
unsafe extern "C" fn rs_apic_local_apic_edge_ack(_irq_num: u8) {
    CurrentApic.send_eoi();
}
