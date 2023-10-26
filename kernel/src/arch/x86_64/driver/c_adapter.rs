use super::{
    hpet::{hpet_init, hpet_instance},
    tsc::TSCManager,
};

#[no_mangle]
unsafe extern "C" fn rs_hpet_init() -> i32 {
    hpet_init()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno())
}

#[no_mangle]
unsafe extern "C" fn rs_hpet_enable() -> i32 {
    hpet_instance()
        .hpet_enable()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno())
}

#[no_mangle]
unsafe extern "C" fn rs_tsc_init() -> i32 {
    TSCManager::init()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno())
}

#[no_mangle]
unsafe extern "C" fn rs_handle_hpet_irq(timer_num: u32) {
    hpet_instance().handle_irq(timer_num);
}
