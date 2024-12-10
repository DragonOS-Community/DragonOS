use crate::{
    driver::clocksource::timer_riscv::riscv_sbi_timer_init_local, exception::InterruptArch,
    sched::SchedArch,
};

use super::CurrentIrqArch;

/// 发起调度
#[no_mangle]
pub extern "C" fn sched() {
    unimplemented!("RiscV64::sched")
}

pub struct RiscV64SchedArch;

impl SchedArch for RiscV64SchedArch {
    fn enable_sched_local() {
        riscv_sbi_timer_init_local();
        unsafe { CurrentIrqArch::interrupt_enable() };
    }

    fn disable_sched_local() {
        todo!()
    }

    fn initial_setup_sched_local() {}
}
