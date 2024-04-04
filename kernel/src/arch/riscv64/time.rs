use crate::time::TimeArch;
pub struct RiscV64TimeArch;

/// 这个是系统jiffies时钟源的固有频率（不是调频之后的）
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 1000000;

impl TimeArch for RiscV64TimeArch {
    fn get_cycles() -> usize {
        riscv::register::cycle::read()
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        todo!()
    }
    /// 将CPU的时钟周期数转换为纳秒
    #[inline(always)]
    fn cycles2ns(cycles: usize) -> usize {
        todo!()
    }
}
