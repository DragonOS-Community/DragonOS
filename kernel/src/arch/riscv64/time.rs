use log::{debug, info};

use crate::{
    driver::open_firmware::fdt::open_firmware_fdt_driver,
    time::{clocksource::HZ, TimeArch},
};
pub struct RiscV64TimeArch;

/// 这个是系统jiffies时钟源的固有频率（不是调频之后的）
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 1000000;

static mut TIME_FREQ: usize = 0;

/// 获取CPU的time寄存器频率
///
/// todo: 支持从acpi中获取
fn init_time_freq() {
    debug!("init_time_freq: init");
    let fdt = open_firmware_fdt_driver().fdt_ref();
    if fdt.is_err() {
        panic!("init_time_freq: failed to get fdt");
    }
    debug!("init_time_freq: get fdt");
    let fdt = fdt.unwrap();
    let cpu_node = fdt.find_node("/cpus");
    if cpu_node.is_none() {
        panic!("init_time_freq: failed to find /cpus node");
    }

    let cpu_node = cpu_node.unwrap();
    let time_freq = cpu_node
        .property("timebase-frequency")
        .map(|prop| prop.as_usize())
        .flatten();
    if time_freq.is_none() {
        panic!("init_time_freq: failed to get timebase-frequency");
    }

    let time_freq: usize = time_freq.unwrap();
    info!("init_time_freq: timebase-frequency: {}", time_freq);
    unsafe {
        TIME_FREQ = time_freq;
    }
}

pub fn time_init() {
    // 初始化cpu time register频率
    init_time_freq();
}

impl TimeArch for RiscV64TimeArch {
    fn get_cycles() -> usize {
        riscv::register::time::read()
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        Self::get_cycles() + ns * unsafe { TIME_FREQ } / 1000000000
    }

    /// 将CPU的时钟周期数转换为纳秒
    #[inline(always)]
    fn cycles2ns(cycles: usize) -> usize {
        if unsafe { TIME_FREQ == 0 } {
            return 0;
        }

        cycles * 1000000000 / unsafe { TIME_FREQ }
    }
}

pub fn riscv_time_base_freq() -> usize {
    unsafe { TIME_FREQ }
}
