use crate::{
    arch::x86_64::asm::current::current_pcb, include::bindings::bindings::process_control_block,
    process::process::process_cpu,
};

use super::cfs::{sched_cfs_init, SchedulerCFS, __get_cfs_scheduler};

/// @brief 获取指定的cpu上正在执行的进程的pcb
#[inline]
pub fn cpu_executing(cpu_id: u32) -> *const process_control_block {
    // todo: 引入per_cpu之后，该函数真正执行“返回指定的cpu上正在执行的pcb”的功能

    if cpu_id == process_cpu(current_pcb()) {
        return current_pcb();
    } else {
        todo!()
    }
}

/// @brief 具体的调度器应当实现的trait
pub trait Scheduler {
    /// @brief 使用该调度器发起调度的时候，要调用的函数
    fn sched(&mut self);

    /// @brief 将pcb加入这个调度器的调度队列
    fn enqueue(&mut self, pcb: &'static mut process_control_block);
}

#[no_mangle]
pub extern "C" fn sched() {
    let cfs_scheduler: &mut SchedulerCFS = __get_cfs_scheduler();

    cfs_scheduler.sched();
}

/// @brief 将进程加入调度队列
pub extern "C" fn sched_enqueue(pcb: &'static mut process_control_block) {
    let cfs_scheduler = __get_cfs_scheduler();
    cfs_scheduler.enqueue(pcb);
}

/// @brief 初始化进程调度器模块
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_init() {
    unsafe {
        sched_cfs_init();
    }
}
