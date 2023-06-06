use core::arch::asm;

use super::asm::current::current_pcb;

/// @brief 获取当前cpu的apic id
#[inline]
pub fn current_cpu_id() -> u32 {
    // TODO: apic重构后，使用apic id来设置这里
    current_pcb().cpu_id as u32
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
