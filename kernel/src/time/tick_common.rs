use crate::{
    arch::interrupt::TrapFrame,
    process::ProcessManager,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    time::timer::run_local_timer,
};

use super::timer::update_timer_jiffies;

/// # 函数的功能
/// 用于周期滴答的事件处理
pub fn tick_handle_periodic(trap_frame: &TrapFrame) {
    let cpu_id = smp_get_processor_id();

    tick_periodic(cpu_id, trap_frame);
}

fn tick_periodic(cpu_id: ProcessorId, trap_frame: &TrapFrame) {
    if cpu_id.data() == 0 {
        update_timer_jiffies(1);
        run_local_timer();
    }

    ProcessManager::update_process_times(trap_frame.is_from_user());
}
