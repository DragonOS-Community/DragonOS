use core::sync::atomic::compiler_fence;

use alloc::{boxed::Box, collections::LinkedList, sync::Arc, vec::Vec};

use crate::{
    arch::cpu::current_cpu_id,
    include::bindings::bindings::MAX_CPU_NUM,
    kBUG, kdebug,
    libs::spinlock::SpinLock,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager},
    smp::cpu::ProcessorId,
};

use super::{
    core::{sched_enqueue, Scheduler},
    SchedPolicy,
};

/// 声明全局的rt调度器实例
pub static mut RT_SCHEDULER_PTR: Option<Box<SchedulerRT>> = None;

/// @brief 获取rt调度器实例的可变引用
#[inline]
pub fn __get_rt_scheduler() -> &'static mut SchedulerRT {
    return unsafe { RT_SCHEDULER_PTR.as_mut().unwrap() };
}

/// @brief 初始化rt调度器
pub unsafe fn sched_rt_init() {
    kdebug!("rt scheduler init");
    if RT_SCHEDULER_PTR.is_none() {
        RT_SCHEDULER_PTR = Some(Box::new(SchedulerRT::new()));
    } else {
        kBUG!("Try to init RT Scheduler twice.");
        panic!("Try to init RT Scheduler twice.");
    }
}
/// @brief RT队列（per-cpu的）
#[derive(Debug)]
struct RTQueue {
    /// 加锁保护的存储进程的双向队列
    locked_queue: SpinLock<LinkedList<Arc<ProcessControlBlock>>>,
}

impl RTQueue {
    pub fn new() -> RTQueue {
        RTQueue {
            locked_queue: SpinLock::new(LinkedList::new()),
        }
    }
    /// @brief 将pcb加入队列
    pub fn enqueue(&mut self, pcb: Arc<ProcessControlBlock>) {
        let mut queue = self.locked_queue.lock_irqsave();

        // 如果进程是IDLE进程，那么就不加入队列
        if pcb.pid().into() == 0 {
            return;
        }
        queue.push_back(pcb);
    }

    /// @brief 将pcb从调度队列头部取出,若队列为空，则返回None
    pub fn dequeue(&mut self) -> Option<Arc<ProcessControlBlock>> {
        let res: Option<Arc<ProcessControlBlock>>;
        let mut queue = self.locked_queue.lock_irqsave();
        if queue.len() > 0 {
            // 队列不为空，返回下一个要执行的pcb
            res = Some(queue.pop_front().unwrap());
        } else {
            // 如果队列为空，则返回None
            res = None;
        }
        return res;
    }
    pub fn enqueue_front(&mut self, pcb: Arc<ProcessControlBlock>) {
        let mut queue = self.locked_queue.lock_irqsave();

        // 如果进程是IDLE进程，那么就不加入队列
        if pcb.pid().into() == 0 {
            return;
        }
        queue.push_front(pcb);
    }

    #[allow(dead_code)]
    pub fn get_rt_queue_size(&mut self) -> usize {
        let queue = self.locked_queue.lock_irqsave();
        return queue.len();
    }
}

/// @brief RT调度器类
pub struct SchedulerRT {
    cpu_queue: Vec<Vec<&'static mut RTQueue>>,
    load_list: Vec<&'static mut LinkedList<u64>>,
}

impl SchedulerRT {
    const RR_TIMESLICE: isize = 100;
    const MAX_RT_PRIO: isize = 100;

    pub fn new() -> SchedulerRT {
        // 暂时手动指定核心数目
        // todo: 从cpu模块来获取核心的数目
        let mut result = SchedulerRT {
            cpu_queue: Default::default(),
            load_list: Default::default(),
        };

        // 为每个cpu核心创建队列
        for cpu_id in 0..MAX_CPU_NUM {
            result.cpu_queue.push(Vec::new());
            // 每个CPU有MAX_RT_PRIO个优先级队列
            for _ in 0..SchedulerRT::MAX_RT_PRIO {
                result.cpu_queue[cpu_id as usize].push(Box::leak(Box::new(RTQueue::new())));
            }
        }
        // 为每个cpu核心创建负载统计队列
        for _ in 0..MAX_CPU_NUM {
            result
                .load_list
                .push(Box::leak(Box::new(LinkedList::new())));
        }
        return result;
    }

    /// @brief 挑选下一个可执行的rt进程
    pub fn pick_next_task_rt(&mut self, cpu_id: ProcessorId) -> Option<Arc<ProcessControlBlock>> {
        // 循环查找，直到找到
        // 这里应该是优先级数量，而不是CPU数量，需要修改
        for i in 0..SchedulerRT::MAX_RT_PRIO {
            let cpu_queue_i: &mut RTQueue = self.cpu_queue[cpu_id.data() as usize][i as usize];
            let proc: Option<Arc<ProcessControlBlock>> = cpu_queue_i.dequeue();
            if proc.is_some() {
                return proc;
            }
        }
        // return 一个空值
        None
    }

    pub fn rt_queue_len(&mut self, cpu_id: ProcessorId) -> usize {
        let mut sum = 0;
        for prio in 0..SchedulerRT::MAX_RT_PRIO {
            sum += self.cpu_queue[cpu_id.data() as usize][prio as usize].get_rt_queue_size();
        }
        return sum as usize;
    }

    #[allow(dead_code)]
    #[inline]
    pub fn load_list_len(&mut self, cpu_id: u32) -> usize {
        return self.load_list[cpu_id as usize].len();
    }

    pub fn enqueue_front(&mut self, pcb: Arc<ProcessControlBlock>) {
        let cpu_id = current_cpu_id().data() as usize;
        let priority = pcb.sched_info().priority().data() as usize;

        self.cpu_queue[cpu_id][priority].enqueue_front(pcb);
    }

    pub fn timer_update_jiffies(&self) {
        ProcessManager::current_pcb()
            .sched_info()
            .increase_rt_time_slice(-1);
    }
}

impl Scheduler for SchedulerRT {
    /// @brief 在当前cpu上进行调度。
    /// 请注意，进入该函数之前，需要关中断
    fn sched(&mut self) -> Option<Arc<ProcessControlBlock>> {
        ProcessManager::current_pcb()
            .flags()
            .remove(ProcessFlags::NEED_SCHEDULE);
        // 正常流程下，这里一定是会pick到next的pcb的，如果是None的话，要抛出错误
        let cpu_id = current_cpu_id();
        let proc: Arc<ProcessControlBlock> =
            self.pick_next_task_rt(cpu_id).expect("No RT process found");
        let priority = proc.sched_info().priority();
        let policy = proc.sched_info().inner_lock_read_irqsave().policy();
        match policy {
            // 如果是fifo策略，则可以一直占有cpu直到有优先级更高的任务就绪(即使优先级相同也不行)或者主动放弃(等待资源)
            SchedPolicy::FIFO => {
                // 如果挑选的进程优先级小于当前进程，则不进行切换
                if proc.sched_info().priority()
                    <= ProcessManager::current_pcb().sched_info().priority()
                {
                    sched_enqueue(proc, false);
                } else {
                    // 将当前的进程加进队列
                    sched_enqueue(ProcessManager::current_pcb(), false);
                    compiler_fence(core::sync::atomic::Ordering::SeqCst);
                    return Some(proc);
                }
            }

            // RR调度策略需要考虑时间片
            SchedPolicy::RR => {
                // 同等优先级的，考虑切换
                if proc.sched_info().priority()
                    >= ProcessManager::current_pcb().sched_info().priority()
                {
                    // 判断这个进程时间片是否耗尽，若耗尽则将其时间片赋初值然后入队
                    if proc.sched_info().rt_time_slice() <= 0 {
                        proc.sched_info()
                            .set_rt_time_slice(SchedulerRT::RR_TIMESLICE as isize);
                        proc.flags().insert(ProcessFlags::NEED_SCHEDULE);
                        sched_enqueue(proc, false);
                    }
                    // 目标进程时间片未耗尽，切换到目标进程
                    else {
                        // 将当前进程加进队列
                        sched_enqueue(ProcessManager::current_pcb(), false);
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                        return Some(proc);
                    }
                }
                // curr优先级更大，说明一定是实时进程，将所选进程入队列，此时需要入队首
                else {
                    self.cpu_queue[cpu_id.data() as usize][priority.data() as usize]
                        .enqueue_front(proc);
                }
            }
            _ => panic!("unsupported schedule policy"),
        }
        return None;
    }

    fn enqueue(&mut self, pcb: Arc<ProcessControlBlock>) {
        let cpu_id = pcb.sched_info().on_cpu().unwrap();
        let cpu_queue = &mut self.cpu_queue[cpu_id.data() as usize];
        let priority = pcb.sched_info().priority().data() as usize;
        cpu_queue[priority].enqueue(pcb);
    }
}
