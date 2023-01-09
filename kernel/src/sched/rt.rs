use core::{ptr::null_mut, sync::atomic::compiler_fence};

use alloc::{boxed::Box, vec::Vec};

use crate::{
    arch::asm::current::current_pcb,
    include::bindings::bindings::{
        initial_proc_union, process_control_block, PF_NEED_SCHED, SCHED_FIFO, SCHED_NORMAL,
        SCHED_RR,
    },
    kBUG, kdebug,
    libs::spinlock::RawSpinlock,
};

use super::core::{sched_enqueue, Scheduler};

/// 声明全局的rt调度器实例

pub static mut RT_SCHEDULER_PTR: *mut SchedulerRT = null_mut();

/// @brief 获取rt调度器实例的可变引用
#[inline]
pub fn __get_rt_scheduler() -> &'static mut SchedulerRT {
    return unsafe { RT_SCHEDULER_PTR.as_mut().unwrap() };
}

/// @brief 初始化rt调度器
pub unsafe fn sched_rt_init() {
    kdebug!("test rt init");
    if RT_SCHEDULER_PTR.is_null() {
        RT_SCHEDULER_PTR = Box::leak(Box::new(SchedulerRT::new()));
    } else {
        kBUG!("Try to init RT Scheduler twice.");
        panic!("Try to init RT Scheduler twice.");
    }
}

/// @brief RT队列（per-cpu的）
#[derive(Debug)]
struct RTQueue {
    /// 队列的锁
    lock: RawSpinlock,
    /// 进程的队列
    queue: Vec<&'static mut process_control_block>,
}

impl RTQueue {
    pub fn new() -> RTQueue {
        RTQueue {
            queue: Vec::new(),
            lock: RawSpinlock::INIT,
        }
    }
    /// @brief 将pcb加入队列
    pub fn enqueue(&mut self, pcb: &'static mut process_control_block) {
        self.lock.lock();

        // 如果进程是IDLE进程，那么就不加入队列
        if pcb.pid == 0 {
            self.lock.unlock();
            return;
        }
        self.queue.push(pcb);
        self.lock.unlock();
    }

    /// @brief 将pcb从调度队列中弹出,若队列为空，则返回IDLE进程的pcb
    pub fn dequeue(&mut self) -> &'static mut process_control_block {
        let res: &'static mut process_control_block;
        self.lock.lock();
        if self.queue.len() > 0 {
            // 队列不为空，返回下一个要执行的pcb
            res = self.queue.pop().unwrap();
        } else {
            // 如果队列为空，则返回IDLE进程的pcb
            res = unsafe { &mut initial_proc_union.pcb };
        }
        self.lock.unlock();
        return res;
    }
}

/// @brief RT调度器类
pub struct SchedulerRT {
    cpu_queue: Vec<&'static mut RTQueue>,
}

impl SchedulerRT {
    const RR_TIMESLICE: i64 = 100;
    const MAX_RT_PRIO: i64 = 100;

    pub fn new() -> SchedulerRT {
        // 暂时手动指定核心数目
        // todo: 从cpu模块来获取核心的数目
        let mut result = SchedulerRT {
            cpu_queue: Default::default(),
        };

        // 为每个cpu核心创建队列
        for _ in 0..SchedulerRT::MAX_RT_PRIO {
            result.cpu_queue.push(Box::leak(Box::new(RTQueue::new())));
        }
        return result;
    }
    /// @brief 挑选下一个可执行的rt进程
    pub fn pick_next_task_rt(&mut self) -> Option<&'static mut process_control_block> {
        // 循环查找，直到找到
        // 这里应该是优先级数量，而不是CPU数量，需要修改
        for i in 0..SchedulerRT::MAX_RT_PRIO {
            let cpu_queue_i: &mut RTQueue = self.cpu_queue[i as usize];
            let proc: &'static mut process_control_block = cpu_queue_i.dequeue();
            if proc.policy != SCHED_NORMAL {
                return Some(proc);
            }
        }
        // return 一个空值
        None
    }
}

impl Scheduler for SchedulerRT {
    /// @brief 在当前cpu上进行调度。
    /// 请注意，进入该函数之前，需要关中断
    fn sched(&mut self) -> Option<&'static mut process_control_block> {
        current_pcb().flags &= !(PF_NEED_SCHED as u64);

        let proc: &'static mut process_control_block =
            self.pick_next_task_rt().expect("No RT process found");

        // 若队列中无下一个进程，则返回

        // 如果是fifo策略，则可以一直占有cpu直到有优先级更高的任务就绪(即使优先级相同也不行)或者主动放弃(等待资源)
        if proc.policy == SCHED_FIFO {
            // 如果挑选的进程优先级小于当前进程，则不进行切换
            if proc.priority <= current_pcb().priority {
                sched_enqueue(proc);
            } else {
                // 将当前的进程加进队列
                sched_enqueue(current_pcb());
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
                return Some(proc);
            }
        }
        // RR调度策略需要考虑时间片
        else if proc.policy == SCHED_RR {
            // 同等优先级的，考虑切换
            if proc.priority >= current_pcb().priority {
                // 判断这个进程时间片是否耗尽，若耗尽则将其时间片赋初值然后入队
                if proc.time_slice <= 0 {
                    proc.time_slice = SchedulerRT::RR_TIMESLICE;
                    proc.flags |= !(PF_NEED_SCHED as u64);
                    sched_enqueue(proc);
                }
                // 目标进程时间片未耗尽，切换到目标进程
                else {
                    proc.time_slice -= 1;
                    // 将当前进程加进队列
                    sched_enqueue(current_pcb());
                    compiler_fence(core::sync::atomic::Ordering::SeqCst);
                    return Some(proc);
                }
            }
            // curr优先级更大，说明一定是实时进程，则减去消耗时间片
            else {
                current_pcb().time_slice -= 1;
                sched_enqueue(proc);
            }
        }
        return None;
    }

    fn enqueue(&mut self, pcb: &'static mut process_control_block) {
        let cpu_queue = &mut self.cpu_queue[pcb.cpu_id as usize];
        cpu_queue.enqueue(pcb);
    }
}
