/// 获取当前cpu的id
#[inline]
pub fn current_cpu_id() -> u32 {
    unimplemented!("RiscV64 current_cpu_id")
}

/// 重置cpu
pub unsafe fn cpu_reset() -> ! {
    sbi_rt::system_reset(sbi_rt::WarmReboot, sbi_rt::NoReason);
    unimplemented!("RiscV64 reset failed, manual override expected ...")
}
