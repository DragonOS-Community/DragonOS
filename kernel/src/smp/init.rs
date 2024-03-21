use crate::smp::{core::smp_get_processor_id, cpu::smp_cpu_manager};

#[inline(never)]
pub fn smp_ap_start_stage2() {
    smp_cpu_manager().complete_ap_thread(true);

    kinfo!("Successfully started AP {}", smp_get_processor_id().data());

    loop {}
}
