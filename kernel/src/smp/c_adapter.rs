use super::{core::smp_get_processor_id, kick_cpu};

#[no_mangle]
pub extern "C" fn rs_kick_cpu(cpu_id: u32) -> usize {
    return kick_cpu(cpu_id)
        .map(|_| 0usize)
        .unwrap_or_else(|e| e.to_posix_errno() as usize);
}

#[no_mangle]
pub extern "C" fn rs_current_cpu_id() -> i32 {
    return smp_get_processor_id() as i32;
}
