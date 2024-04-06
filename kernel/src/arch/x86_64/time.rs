use crate::time::{clocksource::HZ, TimeArch};

use super::driver::tsc::TSCManager;

/// 这个是系统jiffies时钟源的固有频率（不是调频之后的）
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 1000000;

pub fn time_init() {
    // do nothing
}

pub struct X86_64TimeArch;

impl TimeArch for X86_64TimeArch {
    #[inline(always)]
    fn get_cycles() -> usize {
        unsafe { x86::time::rdtsc() as usize }
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        Self::get_cycles() + ns * TSCManager::cpu_khz() as usize / 1000000
    }

    /// 将CPU的时钟周期数转换为纳秒
    #[inline(always)]
    fn cycles2ns(cycles: usize) -> usize {
        cycles * 1000000 / TSCManager::cpu_khz() as usize
    }
}
