use core::ptr::null_mut;

use alloc::{boxed::Box, vec::Vec};

use crate::{
    include::bindings::bindings::process_control_block, kBUG, libs::spinlock::RawSpinlock,
};

use super::core::Scheduler;

/// 声明全局的cfs调度器实例

pub static mut CFS_SCHEDULER_PTR: *mut SchedulerCFS = null_mut();

/// @brief 获取cfs调度器实例的可变引用
#[inline]
pub fn __get_cfs_scheduler() -> &'static mut SchedulerCFS {
    return unsafe { CFS_SCHEDULER_PTR.as_mut().unwrap() };
}

/// @brief 初始化cfs调度器
pub unsafe fn sched_cfs_init() {
    if CFS_SCHEDULER_PTR.is_null() {
        CFS_SCHEDULER_PTR = Box::leak(Box::new(SchedulerCFS::new()));
    } else {
        kBUG!("Try to init CFS Scheduler twice.");
        panic!("Try to init CFS Scheduler twice.");
    }
}

/// @brief CFS队列（per-cpu的）
#[derive(Debug)]
struct CFSQueue {
    /// 当前cpu上执行的进程，剩余的时间片
    cpu_exec_proc_jiffies: i64,
    /// 队列的锁
    lock: RawSpinlock,
    /// 进程的队列
    queue: Vec<&'static mut process_control_block>,
}

impl CFSQueue {
    pub fn new() -> CFSQueue {
        CFSQueue {
            cpu_exec_proc_jiffies: 0,
            lock: RawSpinlock::INIT,
            queue: Vec::new(),
        }
    }

    /// @brief 将进程按照虚拟运行时间的升序进行排列
    /// todo: 换掉这个sort方法，因为它底层是归并很占内存，且时间复杂度为nlogn，（遍历然后插入的方法，时间复杂度最坏是n）
    pub fn sort(&mut self) {
        self.queue
            .sort_by(|a, b| (*a).virtual_runtime.cmp(&(*b).virtual_runtime));
    }
}

/// @brief CFS调度器类
pub struct SchedulerCFS {
    cpu_queue: Vec<&'static mut CFSQueue>,
}

impl SchedulerCFS {
    pub fn new() -> SchedulerCFS {
        // 暂时手动指定核心数目
        // todo: 从cpu模块来获取核心的数目
        let cpu_count = 64;
        let mut result = SchedulerCFS {
            cpu_queue: Default::default(),
        };

        // 为每个cpu核心创建队列
        for _ in 0..cpu_count {
            result.cpu_queue.push(Box::leak(Box::new(CFSQueue::new())));
        }

        return result;
    }
}
impl Scheduler for SchedulerCFS {
    fn sched(&mut self) {}

    fn enqueue(&mut self, pcb: &'static mut process_control_block) {
        let cpu_queue = &mut self.cpu_queue[pcb.cpu_id as usize];
        cpu_queue.lock.lock();
        cpu_queue.queue.push(pcb);
        cpu_queue.sort();
        cpu_queue.lock.unlock();
    }
}
