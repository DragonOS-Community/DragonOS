use crate::time::{clocksource::HZ, TimeArch};

use super::driver::tsc::TSCManager;

/// 这个是系统jiffies时钟源的固有频率（不是调频之后的）
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 1000000;

pub fn time_init() {
    // do nothing
}

pub struct X86_64TimeArch;

#[inline(always)]
fn scale_by_ratio(value: usize, numerator: usize, denominator: usize) -> usize {
    debug_assert_ne!(denominator, 0);
    // `remainder < denominator`, so this proves that the only ordinary
    // multiplication cannot overflow. The quotient term intentionally wraps
    // with the architecture cycle counter, matching TimeArch's API.
    debug_assert!(numerator <= usize::MAX / denominator);
    let quotient = value / denominator;
    let remainder = value - quotient * denominator;
    quotient
        .wrapping_mul(numerator)
        .wrapping_add(remainder * numerator / denominator)
}

#[inline(always)]
pub(crate) fn cycles_to_ns(cycles: usize, cpu_khz: u64) -> usize {
    scale_by_ratio(cycles, 1_000_000, cpu_khz as usize)
}

impl TimeArch for X86_64TimeArch {
    #[inline(always)]
    fn get_cycles() -> usize {
        unsafe { x86::time::rdtsc() as usize }
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        let delta = scale_by_ratio(ns, TSCManager::cpu_khz() as usize, 1_000_000);
        Self::get_cycles().wrapping_add(delta)
    }

    /// 将CPU的时钟周期数转换为纳秒
    #[inline(always)]
    fn cycles2ns(cycles: usize) -> usize {
        cycles_to_ns(cycles, TSCManager::cpu_khz())
    }
}
