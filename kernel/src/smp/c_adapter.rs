use super::kick_cpu;

#[no_mangle]
pub extern "C" fn rs_kick_cpu(cpu_id: usize) -> usize {
    return kick_cpu(cpu_id)
        .map(|_| 0usize)
        .unwrap_or_else(|e| e.to_posix_errno() as usize);
}

#[no_mangle]
pub unsafe extern "C" fn rs_smp_init_idle() {
    crate::smp::init_smp_idle_process();
}
