use crate::time::{clocksource::HZ, TimeArch};

use super::driver::tsc::TSCManager;

/// 这个是系统jiffies时钟源的固有频率（不是调频之后的）
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 1000000;

pub fn time_init() {
    // do nothing
}

pub struct X86_64TimeArch;

#[inline(always)]
pub(crate) fn cycles_to_ns(cycles: usize, cpu_khz: u64) -> usize {
    debug_assert_ne!(cpu_khz, 0);
    ((cycles as u128 * 1_000_000u128) / cpu_khz as u128) as usize
}

impl TimeArch for X86_64TimeArch {
    #[inline(always)]
    fn get_cycles() -> usize {
        unsafe { x86::time::rdtsc() as usize }
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        let delta = (ns as u128 * TSCManager::cpu_khz() as u128 / 1_000_000u128) as usize;
        Self::get_cycles().wrapping_add(delta)
    }

    /// 将CPU的时钟周期数转换为纳秒
    #[inline(always)]
    fn cycles2ns(cycles: usize) -> usize {
        cycles_to_ns(cycles, TSCManager::cpu_khz())
    }
}
