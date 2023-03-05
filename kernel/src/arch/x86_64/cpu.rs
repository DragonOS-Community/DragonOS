use core::arch::asm;

/// @brief 获取当前cpu的apic id
#[inline]
pub fn current_cpu_id() -> u8 {
    let cpuid_res: u32;
    unsafe {
        asm!(
             "mov eax, 1",
             "cpuid",
             "mov r15, rbx",
             lateout("r15") cpuid_res
        );
    }
    return ((cpuid_res >> 24) & 0xff) as u8;
}

/// @brief 通过pause指令，让cpu休息一会儿。降低空转功耗
pub fn cpu_relax() {
    unsafe {
        asm!("pause");
    }
}
