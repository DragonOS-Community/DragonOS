use core::arch::asm;

use x86::cpuid::{cpuid, CpuIdResult};

/// @brief 获取当前cpu的apic id
#[inline]
pub fn current_cpu_id() -> u32 {
    let cpuid_res: CpuIdResult = cpuid!(0x1);
    let cpu_id = (cpuid_res.ebx >> 24) & 0xff;
    return cpu_id;
}

/// @brief 通过pause指令，让cpu休息一会儿。降低空转功耗
pub fn cpu_relax() {
    unsafe {
        asm!("pause");
    }
}

/// 重置cpu
pub fn cpu_reset() -> ! {
    // 重启计算机
    unsafe { x86::io::outb(0x64, 0xfe) };
    loop {}
}
