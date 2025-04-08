use crate::smp::cpu::ProcessorId;

/// 重置cpu
pub unsafe fn cpu_reset() -> ! {
    todo!("la64:cpu_reset")
}

/// 获取当前cpu的id
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    todo!("la64:current_cpu_id")
}
