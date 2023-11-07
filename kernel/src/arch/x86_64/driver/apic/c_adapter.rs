use super::{
    apic_timer::{LocalApicTimer, LocalApicTimerIntrController},
    ioapic::{ioapic_disable, ioapic_enable, ioapic_install, ioapic_uninstall},
    CurrentApic, LocalAPIC,
};

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_install(irq_num: u8) {
    LocalApicTimerIntrController.install(irq_num);
}

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_uninstall(_irq_num: u8) {
    LocalApicTimerIntrController.uninstall();
}

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_enable(_irq_num: u8) {
    LocalApicTimerIntrController.enable();
}

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_disable(_irq_num: u8) {
    LocalApicTimerIntrController.disable();
}

#[no_mangle]
unsafe extern "C" fn rs_apic_local_apic_edge_ack(_irq_num: u8) {
    CurrentApic.send_eoi();
}

/// 初始化bsp处理器的apic
#[no_mangle]
pub extern "C" fn rs_apic_init_bsp() -> i32 {
    if CurrentApic.init_current_cpu() {
        return 0;
    }

    return -1;
}

#[no_mangle]
pub extern "C" fn rs_apic_init_ap() -> i32 {
    if CurrentApic.init_current_cpu() {
        return 0;
    }

    return -1;
}

#[no_mangle]
unsafe extern "C" fn rs_ioapic_install(
    vector: u8,
    dest: u8,
    level_triggered: bool,
    active_high: bool,
    dest_logic: bool,
) -> i32 {
    return ioapic_install(vector, dest, level_triggered, active_high, dest_logic)
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}

#[no_mangle]
unsafe extern "C" fn rs_ioapic_uninstall(vector: u8) {
    ioapic_uninstall(vector);
}

#[no_mangle]
unsafe extern "C" fn rs_ioapic_enable(vector: u8) {
    ioapic_enable(vector);
}

#[no_mangle]
unsafe extern "C" fn rs_ioapic_disable(vector: u8) {
    ioapic_disable(vector);
}

#[no_mangle]
unsafe extern "C" fn rs_apic_timer_handle_irq(_irq_num: u8) -> i32 {
    return LocalApicTimer::handle_irq()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}
