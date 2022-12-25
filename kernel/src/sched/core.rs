use crate::{
    arch::x86_64::asm::current::current_pcb, include::bindings::bindings::process_control_block,
    process::process::process_cpu,
};

use super::cfs::{scheduler_cfs, SchedulerCFS};

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

/// @brief 抽象调度器类，作用是用来包裹特定的调度器。（否则难以实现lazy static)
///
/// 请注意，每个具体的调度器需要为包裹着它的那个AbstractScheduler实现一个initialize方法，以支持初始化内部的具体调度器。
#[derive(Debug)]
pub struct AbstractScheduler<T> {
    pub sched: Option<T>,
}

impl<T> AbstractScheduler<T> {
    /// @brief 返回一个未被初始化的GlobalScheduler对象
    pub const fn uninitialized() -> AbstractScheduler<T> {
        return AbstractScheduler { sched: None };
    }

    pub unsafe fn initialize(&mut self, scheduler: T) {
        self.sched.replace(scheduler);
    }
}

/// @brief 这是内层的，具体的调度器应当实现的trait
pub trait SchedulerAction {
    /// @brief 具体的调度器的初始化方法
    fn initialize();

    /// @brief 使用该调度器发起调度的时候，要调用的函数
    fn sched();

    /// @brief 将pcb加入这个调度器的调度队列
    fn enqueue(pcb: &'static mut process_control_block);
}

pub extern "C" fn sched() {}

pub extern "C" fn sched_init() {
    unsafe {
        scheduler_cfs.initialize(SchedulerCFS::new());
    }
}
