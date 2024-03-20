use x86::cpuid::{cpuid, CpuIdResult};

use crate::smp::cpu::{ProcessorId, SmpCpuManager};

/// 获取当前cpu的apic id
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    let cpuid_res: CpuIdResult = cpuid!(0x1);
    let cpu_id = (cpuid_res.ebx >> 24) & 0xff;
    return ProcessorId::new(cpu_id);
}

/// 重置cpu
pub unsafe fn cpu_reset() -> ! {
    // 重启计算机
    unsafe { x86::io::outb(0x64, 0xfe) };
    loop {}
}

impl SmpCpuManager {
    pub fn arch_init(_boot_cpu: ProcessorId) {}
}
