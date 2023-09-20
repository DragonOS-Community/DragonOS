/// @brief 获取当前的cpu id
#[inline]
pub fn smp_get_processor_id() -> u32 {
    return crate::arch::cpu::current_cpu_id();
}
