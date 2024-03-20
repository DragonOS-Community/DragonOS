use crate::smp::cpu::ProcessorId;

use super::ipi::{ipi_send_smp_init, ipi_send_smp_startup};

#[no_mangle]
unsafe extern "C" fn rs_ipi_send_smp_init() -> i32 {
    return ipi_send_smp_init()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}

#[no_mangle]
unsafe extern "C" fn rs_ipi_send_smp_startup(target_cpu: u32) -> i32 {
    return ipi_send_smp_startup(ProcessorId::new(target_cpu))
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}
