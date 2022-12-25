use core::cell::Cell;

use crate::include::bindings::bindings::process_control_block;

use super::core::{AbstractScheduler, SchedulerAction};

/// 声明全局的cfs调度器实例
#[allow(non_upper_case_globals)]
pub static mut scheduler_cfs: AbstractScheduler<SchedulerCFS> =
    AbstractScheduler::<SchedulerCFS>::uninitialized();

pub struct SchedulerCFS {
    
}

impl SchedulerCFS{
    pub fn new()->SchedulerCFS{
        SchedulerCFS{}
    }
}
impl SchedulerAction for SchedulerCFS{
    fn initialize() {
        
    }

    fn sched() {
        
    }

    fn enqueue(pcb: &'static mut crate::include::bindings::bindings::process_control_block) {
        
    }
}

impl<SchedulerCFS> AbstractScheduler<SchedulerCFS>{
    pub fn enqueue(&mut self, pcb: &'static mut process_control_block){
        
        return;
    }
}