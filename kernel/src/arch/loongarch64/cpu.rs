use crate::smp::cpu::ProcessorId;

/// 重置cpu
pub unsafe fn cpu_reset() -> ! {
    log::warn!("cpu_reset on loongarch64 platform was not implemented!");
    loop {
        unsafe { loongArch64::asm::idle() };
    }
}

/// 获取当前cpu的id
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    ProcessorId::new(loongArch64::register::cpuid::read().core_id() as u32)
}
