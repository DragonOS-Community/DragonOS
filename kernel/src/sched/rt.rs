use core::{ptr::null_mut, sync::atomic::compiler_fence};

use alloc::{boxed::Box, collections::LinkedList, vec::Vec};

use crate::{
    arch::asm::current::current_pcb,
    include::bindings::bindings::{
        process_control_block, MAX_CPU_NUM, PF_NEED_SCHED, SCHED_FIFO, SCHED_RR,
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
    kdebug!("rt scheduler init");
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
    /// 存储进程的双向队列
    queue: LinkedList<&'static mut process_control_block>,
}

impl RTQueue {
    pub fn new() -> RTQueue {
        RTQueue {
            queue: LinkedList::new(),
            lock: RawSpinlock::INIT,
        }
    }
    /// @brief 将pcb加入队列
    pub fn enqueue(&mut self, pcb: &'static mut process_control_block) {
        let mut rflags = 0u64;
        self.lock.lock_irqsave(&mut rflags);

        // 如果进程是IDLE进程，那么就不加入队列
        if pcb.pid == 0 {
            self.lock.unlock_irqrestore(&rflags);
            return;
        }
        self.queue.push_back(pcb);
        self.lock.unlock_irqrestore(&rflags);
    }

    /// @brief 将pcb从调度队列头部取出,若队列为空，则返回None
    pub fn dequeue(&mut self) -> Option<&'static mut process_control_block> {
        let res: Option<&'static mut process_control_block>;
        let mut rflags = 0u64;
        self.lock.lock_irqsave(&mut rflags);
        if self.queue.len() > 0 {
            // 队列不为空，返回下一个要执行的pcb
            res = Some(self.queue.pop_front().unwrap());
        } else {
            // 如果队列为空，则返回None
            res = None;
        }
        self.lock.unlock_irqrestore(&rflags);
        return res;
    }
    pub fn enqueue_front(&mut self, pcb: &'static mut process_control_block) {
        let mut rflags = 0u64;
        self.lock.lock_irqsave(&mut rflags);

        // 如果进程是IDLE进程，那么就不加入队列
        if pcb.pid == 0 {
            self.lock.unlock_irqrestore(&rflags);
            return;
        }
        self.queue.push_front(pcb);
        self.lock.unlock_irqrestore(&rflags);
    }
    pub fn get_rt_queue_size(&mut self) -> usize {
        return self.queue.len();
    }
}

/// @brief RT调度器类
pub struct SchedulerRT {
    cpu_queue: Vec<Vec<&'static mut RTQueue>>,
    load_list: Vec<&'static mut LinkedList<u64>>,
}

impl SchedulerRT {
    const RR_TIMESLICE: i64 = 100;
    const MAX_RT_PRIO: i64 = 100;

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
    pub fn pick_next_task_rt(&mut self, cpu_id: u32) -> Option<&'static mut process_control_block> {
        // 循环查找，直到找到
        // 这里应该是优先级数量，而不是CPU数量，需要修改
        for i in 0..SchedulerRT::MAX_RT_PRIO {
            let cpu_queue_i: &mut RTQueue = self.cpu_queue[cpu_id as usize][i as usize];
            let proc: Option<&'static mut process_control_block> = cpu_queue_i.dequeue();
            if proc.is_some() {
                return proc;
            }
        }
        // return 一个空值
        None
    }

    pub fn rt_queue_len(&mut self, cpu_id: u32) -> usize {
        let mut sum = 0;
        for prio in 0..SchedulerRT::MAX_RT_PRIO {
            sum += self.cpu_queue[cpu_id as usize][prio as usize].get_rt_queue_size();
        }
        return sum as usize;
    }

    #[inline]
    pub fn load_list_len(&mut self, cpu_id: u32) -> usize {
        return self.load_list[cpu_id as usize].len();
    }

    pub fn enqueue_front(&mut self, pcb: &'static mut process_control_block) {
        self.cpu_queue[pcb.cpu_id as usize][pcb.priority as usize].enqueue_front(pcb);
    }
}

impl Scheduler for SchedulerRT {
    /// @brief 在当前cpu上进行调度。
    /// 请注意，进入该函数之前，需要关中断
    fn sched(&mut self) -> Option<&'static mut process_control_block> {
        current_pcb().flags &= !(PF_NEED_SCHED as u64);
        // 正常流程下，这里一定是会pick到next的pcb的，如果是None的话，要抛出错误
        let cpu_id = current_pcb().cpu_id;
        let proc: &'static mut process_control_block =
            self.pick_next_task_rt(cpu_id).expect("No RT process found");

        // 如果是fifo策略，则可以一直占有cpu直到有优先级更高的任务就绪(即使优先级相同也不行)或者主动放弃(等待资源)
        if proc.policy == SCHED_FIFO {
            // 如果挑选的进程优先级小于当前进程，则不进行切换
            if proc.priority <= current_pcb().priority {
                sched_enqueue(proc, false);
            } else {
                // 将当前的进程加进队列
                sched_enqueue(current_pcb(), false);
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
                return Some(proc);
            }
        }
        // RR调度策略需要考虑时间片
        else if proc.policy == SCHED_RR {
            // 同等优先级的，考虑切换
            if proc.priority >= current_pcb().priority {
                // 判断这个进程时间片是否耗尽，若耗尽则将其时间片赋初值然后入队
                if proc.rt_time_slice <= 0 {
                    proc.rt_time_slice = SchedulerRT::RR_TIMESLICE;
                    proc.flags |= !(PF_NEED_SCHED as u64);
                    sched_enqueue(proc, false);
                }
                // 目标进程时间片未耗尽，切换到目标进程
                else {
                    // 将当前进程加进队列
                    sched_enqueue(current_pcb(), false);
                    compiler_fence(core::sync::atomic::Ordering::SeqCst);
                    return Some(proc);
                }
            }
            // curr优先级更大，说明一定是实时进程，将所选进程入队列，此时需要入队首
            else {
                self.cpu_queue[cpu_id as usize][proc.cpu_id as usize].enqueue_front(proc);
            }
        }
        return None;
    }

    fn enqueue(&mut self, pcb: &'static mut process_control_block) {
        let cpu_id = pcb.cpu_id;
        let cpu_queue = &mut self.cpu_queue[pcb.cpu_id as usize];
        cpu_queue[cpu_id as usize].enqueue(pcb);
        // // 获取当前时间
        // let time = unsafe { _rdtsc() };
        // let freq = unsafe { Cpu_tsc_freq };
        // // kdebug!("this is timeeeeeeer {},freq is {}, {}", time, freq, cpu_id);
        // // 将当前时间加入负载记录队列
        // self.load_list[cpu_id as usize].push_back(time);
        // // 如果队首元素与当前时间差超过设定值，则移除队首元素
        // while self.load_list[cpu_id as usize].len() > 1
        //     && (time - *self.load_list[cpu_id as usize].front().unwrap() > 10000000000)
        // {
        //     self.load_list[cpu_id as usize].pop_front();
        // }
    }
}
