/// @brief 获取当前的cpu id
#[inline]
pub fn smp_get_processor_id() -> u32 {
    if cfg!(x86_64) {
        return crate::arch::x86_64::cpu::arch_current_apic_id() as u32;
    } else {
        255
    }
}

#[inline]
pub fn smp_send_reschedule(_cpu: u32) {
    // todo:
}
