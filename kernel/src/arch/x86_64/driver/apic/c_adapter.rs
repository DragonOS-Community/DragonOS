use super::{CurrentApic, LocalAPIC};

#[no_mangle]
pub extern "C" fn rs_apic_init_ap() -> i32 {
    if CurrentApic.init_current_cpu() {
        return 0;
    }

    return -1;
}
