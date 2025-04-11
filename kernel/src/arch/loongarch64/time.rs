use crate::time::{clocksource::HZ, TimeArch};

/// 这个是系统jiffies时钟源的固有频率（不是调频之后的）
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 1000000;

pub struct LoongArch64TimeArch;

impl TimeArch for LoongArch64TimeArch {
    fn get_cycles() -> usize {
        todo!("LoongArch64TimeArch::get_cycles")
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        todo!("LoongArch64TimeArch::cal_expire_cycles")
    }

    fn cycles2ns(cycles: usize) -> usize {
        todo!("LoongArch64TimeArch::cycles2ns")
    }
}

pub fn time_init() {
    todo!("la64:time_init");
}
