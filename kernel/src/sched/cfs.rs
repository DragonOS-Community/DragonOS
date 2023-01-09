use core::{
    ptr::null_mut,
    sync::atomic::compiler_fence,
};

use alloc::{boxed::Box, vec::Vec};

use crate::{
    arch::{
        asm::current::current_pcb,
        context::switch_process,
    },
    include::bindings::bindings::{
        initial_proc_union, process_control_block, MAX_CPU_NUM, PF_NEED_SCHED,
        PROC_RUNNING,SCHED_FIFO,SCHED_RR,SCHED_NORMAL
    },
    kBUG,
    kdebug,
    libs::spinlock::RawSpinlock,
};

use super::core::Scheduler;
use super::rt::{sched_RT_init,SchedulerRT,__get_rt_scheduler};

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

    /// @brief 将pcb加入队列
    pub fn enqueue(&mut self, pcb: &'static mut process_control_block) {
        self.lock.lock();

        // 如果进程是IDLE进程，那么就不加入队列
        if pcb.pid == 0 {
            self.lock.unlock();
            return;
        }
        self.queue.push(pcb);
        self.sort();
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

/// @brief CFS调度器类
pub struct SchedulerCFS {
    cpu_queue: Vec<&'static mut CFSQueue>,
}

impl SchedulerCFS {
    pub fn new() -> SchedulerCFS {
        // 暂时手动指定核心数目
        // todo: 从cpu模块来获取核心的数目
        let mut result = SchedulerCFS {
            cpu_queue: Default::default(),
        };

        // 为每个cpu核心创建队列
        for _ in 0..MAX_CPU_NUM {
            result.cpu_queue.push(Box::leak(Box::new(CFSQueue::new())));
        }

        return result;
    }

    /// @brief 更新这个cpu上，这个进程的可执行时间。
    #[inline]
    fn update_cpu_exec_proc_jiffies(_priority: i64, cfs_queue: &mut CFSQueue) -> &mut CFSQueue {
        // todo: 引入调度周期以及所有进程的优先权进行计算，然后设置分配给进程的可执行时间
        cfs_queue.cpu_exec_proc_jiffies = 10;

        return cfs_queue;
    }

    /// @brief 时钟中断到来时，由sched的core模块中的函数，调用本函数，更新CFS进程的可执行时间
    pub fn timer_update_jiffies(&mut self) {
        let current_cpu_queue: &mut CFSQueue = self.cpu_queue[current_pcb().cpu_id as usize];
        // todo: 引入调度周期以及所有进程的优先权进行计算，然后设置进程的可执行时间

        // 更新进程的剩余可执行时间
        current_cpu_queue.lock.lock();
        current_cpu_queue.cpu_exec_proc_jiffies -= 1;
        // 时间片耗尽，标记需要被调度
        if current_cpu_queue.cpu_exec_proc_jiffies <= 0 {
            current_pcb().flags |= PF_NEED_SCHED as u64;
        }
        current_cpu_queue.lock.unlock();

        // 更新当前进程的虚拟运行时间
        current_pcb().virtual_runtime += 1;
    }
}

impl Scheduler for SchedulerCFS {
    /// @brief 在当前cpu上进行调度。
    /// 请注意，进入该函数之前，需要关中断
    fn sched(&mut self) {
        // kdebug!("cfs:sched");
        current_pcb().flags &= !(PF_NEED_SCHED as u64);
        let current_cpu_id = current_pcb().cpu_id as usize;
        let rt_scheduler: &mut SchedulerRT = __get_rt_scheduler();
        let current_cpu_queue: &mut CFSQueue = self.cpu_queue[current_cpu_id];
        let proc: &'static mut process_control_block = current_cpu_queue.dequeue();
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 如果当前不是running态，或者当前进程的虚拟运行时间大于等于下一个进程的，那就需要切换。
        if (current_pcb().state & (PROC_RUNNING as u64)) == 0
            || current_pcb().virtual_runtime >= proc.virtual_runtime
        {
            // kdebug!("cfs:sched 2 state {} running {} pid {}",current_pcb().state,PROC_RUNNING,current_pcb().pid);
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            // 本次切换由于时间片到期引发，则再次加入就绪队列，否则交由其它功能模块进行管理
            if current_pcb().state & (PROC_RUNNING as u64) != 0 {
                kdebug!("cfs:sched->enqueue");
                // current_cpu_queue.enqueue(current_pcb());

                if current_pcb().policy==SCHED_RR || current_pcb().policy==SCHED_FIFO{
                    rt_scheduler.enqueue(current_pcb());
                }
                else{
                    current_cpu_queue.enqueue(current_pcb());
                }
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
            }

            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            // kdebug!("cfs:sched 21");
        // 设置进程可以执行的时间
            if current_cpu_queue.cpu_exec_proc_jiffies <= 0 {
                // kdebug!("cfs:sched 22");
                SchedulerCFS::update_cpu_exec_proc_jiffies(proc.priority, current_cpu_queue);
            }

            kdebug!("cfs:switch_process from {} to {}",current_pcb().pid,proc.pid);

            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            
            switch_process(current_pcb(), proc);
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
        } else {
            kdebug!("cfs:sched not change");
            // 不进行切换

            // 设置进程可以执行的时间
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            if current_cpu_queue.cpu_exec_proc_jiffies <= 0 {
                SchedulerCFS::update_cpu_exec_proc_jiffies(proc.priority, current_cpu_queue);
            }

            compiler_fence(core::sync::atomic::Ordering::SeqCst);

            // current_cpu_queue.enqueue(proc);
            if current_pcb().policy==SCHED_RR || current_pcb().policy==SCHED_FIFO{
                rt_scheduler.enqueue(proc);
            }
            else{
                current_cpu_queue.enqueue(proc);
            }
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }

    fn enqueue(&mut self, pcb: &'static mut process_control_block) {
        let cpu_queue = &mut self.cpu_queue[pcb.cpu_id as usize];
        cpu_queue.enqueue(pcb);
    }
}
