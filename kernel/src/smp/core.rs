use super::cpu::ProcessorId;

/// @brief 获取当前的cpu id
#[inline]
pub fn smp_get_processor_id() -> ProcessorId {
    return crate::arch::cpu::current_cpu_id();
}
