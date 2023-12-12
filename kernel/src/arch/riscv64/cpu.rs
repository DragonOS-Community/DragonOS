/// 获取当前cpu的id
#[inline]
pub fn current_cpu_id() -> u32 {
    unimplemented!("RiscV64 current_cpu_id")
}

/// 重置cpu
pub unsafe fn cpu_reset() -> ! {
    unimplemented!("RiscV64 cpu_reset")
}
