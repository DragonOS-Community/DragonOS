use super::hpet::hpet_instance;

#[no_mangle]
unsafe extern "C" fn rs_handle_hpet_irq(timer_num: u32) {
    hpet_instance().handle_irq(timer_num);
}
