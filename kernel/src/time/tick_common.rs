use crate::{
    arch::interrupt::TrapFrame,
    process::ProcessManager,
    sched::loadavg,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    time::timer::run_local_timer,
};

use super::{
    timekeeping,
    timer::{clock, timer_tick_maintenance},
};

/// Called on every real clockevent IRQ, including early precise events.
pub(crate) fn tick_handle_global(local_tick: bool) {
    if smp_get_processor_id().data() != 0 {
        return;
    }
    let advanced = timekeeping::advance_jiffies(local_tick);
    if advanced || local_tick {
        timer_tick_maintenance();
        loadavg::calc_global_load(clock());
        run_local_timer();
    }
}

/// # 函数的功能
/// 用于周期滴答的事件处理
pub fn tick_handle_periodic(trap_frame: &TrapFrame) {
    let cpu_id = smp_get_processor_id();

    tick_periodic(cpu_id, trap_frame);
}

fn tick_periodic(cpu_id: ProcessorId, trap_frame: &TrapFrame) {
    crate::sched::clock::SchedClock::tick(cpu_id);
    crate::rcu::scheduler_tick();
    ProcessManager::update_process_times(trap_frame.is_from_user());
}
