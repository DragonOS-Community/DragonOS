// use core::{
//     arch::{asm, x86_64::_mm_mfence},
//     ptr::{null_mut, read_volatile},
//     sync::atomic::compiler_fence,
// };

// use alloc::{boxed::Box, vec::Vec};

// use crate::{
//     arch::{
//         asm::current::current_pcb,
//         context::switch_process,
//         mm::{barrier::mfence, switch_mm},
//     },
//     include::bindings::bindings::{
//         initial_proc_union, process_control_block, pt_regs, MAX_CPU_NUM, PF_NEED_SCHED,
//         PROC_RUNNING,SCHED_FIFO,SCHED_RR
//     },
//     kBUG, kdebug,
//     libs::spinlock::RawSpinlock,
//     println,
// };

// use super::core::Scheduler;

// const RR_TIMESLICE: i64 = 100;
// /// 声明全局的rt调度器实例

// pub static mut RT_SCHEDULER_PTR: *mut SchedulerRT = null_mut();

// /// @brief 获取rt调度器实例的可变引用
// #[inline]
// pub fn __get_rt_scheduler() -> &'static mut SchedulerRT {
//     return unsafe { RT_SCHEDULER_PTR.as_mut().unwrap() };
// }

// /// @brief 初始化rt调度器
// pub unsafe fn sched_RT_init() {
//     kdebug!("test rt init");
//     if RT_SCHEDULER_PTR.is_null() {
//         RT_SCHEDULER_PTR = Box::leak(Box::new(SchedulerRT::new()));
//     } else {
//         kBUG!("Try to init RT Scheduler twice.");
//         panic!("Try to init RT Scheduler twice.");
//     }
// }

// /// @brief RT队列（per-cpu的）
// #[derive(Debug)]
// struct RTQueue {
//     /// 队列的锁
//     lock: RawSpinlock,
//     /// 进程的队列
//     queue: Vec<&'static mut process_control_block>,
//     rt_queued:i32,
// }

// impl RTQueue {
//     pub fn new() -> RTQueue {
//         RTQueue {
//             queue: Vec::new(),
//             lock: RawSpinlock::INIT,
//             rt_queued: 0,
//         }
//     }

//     /// @brief 将进程按照虚拟运行时间的升序进行排列
//     /// todo: 换掉这个sort方法，因为它底层是归并很占内存，且时间复杂度为nlogn，（遍历然后插入的方法，时间复杂度最坏是n）
//     pub fn sort(&mut self) {
//         self.queue
//             .sort_by(|a, b| (*a).virtual_runtime.cmp(&(*b).virtual_runtime));
//     }

//     /// @brief 将pcb加入队列
//     pub fn enqueue(&mut self, pcb: &'static mut process_control_block) {
//         self.lock.lock();

//         // 如果进程是IDLE进程，那么就不加入队列
//         if pcb.pid == 0 {
//             self.lock.unlock();
//             return;
//         }
//         self.queue.push(pcb);
//         self.sort();
//         self.lock.unlock();
//     }

//     /// @brief 将pcb从调度队列中弹出,若队列为空，则返回IDLE进程的pcb
//     pub fn dequeue(&mut self) -> &'static mut process_control_block {
//         let res: &'static mut process_control_block;
//         self.lock.lock();
//         if self.queue.len() > 0 {
//             // 队列不为空，返回下一个要执行的pcb
//             res = self.queue.pop().unwrap();
//         } else {
//             // 如果队列为空，则返回IDLE进程的pcb
//             res = unsafe { &mut initial_proc_union.pcb };
//         }
//         self.lock.unlock();
//         return res;
//     }
// }

// /// @brief RT调度器类
// pub struct SchedulerRT {
//     cpu_queue: Vec<&'static mut RTQueue>,
// }

// impl SchedulerRT {
//     pub fn new() -> SchedulerRT {
//         // 暂时手动指定核心数目
//         // todo: 从cpu模块来获取核心的数目
//         let mut result = SchedulerRT {
//             cpu_queue: Default::default(),
//         };

//         // 为每个cpu核心创建队列
//         for _ in 0..MAX_CPU_NUM {
//             result.cpu_queue.push(Box::leak(Box::new(RTQueue::new())));
//         }
//         return result;
//     }
//     /// @brief 挑选下一个可执行的rt进程
//     pub fn pick_next_task_rt(&mut self) -> &'static mut process_control_block{
//         // 循环查找，直到找到
//         let current_cpu_id = current_pcb().cpu_id as usize;
//         let current_cpu_queue: &mut RTQueue = self.cpu_queue[current_cpu_id];
//         let proc: &'static mut process_control_block = current_cpu_queue.dequeue();
//         proc
//     }
//     pub fn enqueue_task_rt(&mut self, cpu_id:usize, pcb: &'static mut process_control_block) {
//         let cpu_queue: &mut RTQueue = self.cpu_queue[cpu_id];
//         cpu_queue.enqueue(pcb);
//     }
// }

// impl Scheduler for SchedulerRT {
//     /// @brief 在当前cpu上进行调度。
//     /// 请注意，进入该函数之前，需要关中断
//     fn sched(&mut self) {
//         // kdebug!("RT:sched");
//         let mut need_change:bool=false;
//         current_pcb().flags &= !(PF_NEED_SCHED as u64);

//         let proc: &'static mut process_control_block =self.pick_next_task_rt();
//         // 若队列中无下一个进程，则返回

//         // if(proc==NULL){
//         //     return ;
//         // }
//         // 如果是fifo策略，则可以一直占有cpu直到有优先级更高的任务就绪(即使优先级相同也不行)或者主动放弃(等待资源)
//         if proc.policy == SCHED_FIFO {
//             // 如果挑选的进程优先级小于当前进程，则不进行切换
//             if proc.priority <= current_pcb().priority{
//                 // enqueue_task_rt(&rq_tmp, proc, 0);
//                 self.enqueue_task_rt(proc.priority as usize, proc);
//             }
//             else{
//                 need_change = true;
//             }
//         }
//         // RR调度策略需要考虑时间片
//         else if proc.policy == SCHED_RR{
//             if proc.priority > current_pcb().priority {
//                 // 判断这个进程时间片是否耗尽，若耗尽则将其时间片赋初值然后入队
//                 if proc.time_slice <= 0 {
//                     proc.time_slice = RR_TIMESLICE;
//                     proc.flags |= !(PF_NEED_SCHED as u64);
//                     self.enqueue_task_rt(proc.priority as usize, proc);
//                     // kinfo("sched_rt:if after rt proc.rt_se.time_slice %d", proc.rt_se.time_slice);

//                 }
//                 // 目标进程时间片未耗尽，切换到目标进程
//                 else
//                 {
//                     proc.time_slice-=1;
//                     // kinfo("sched_rt:else after rt proc.rt_se.time_slice %d", proc.rt_se.time_slice);
//                     need_change = true;
//                 }
//             }
//             // curr优先级更大，说明一定是实时进程，则减去消耗时间片
//             else {
//                 // kinfo("sched_rt:if else after rt proc->rt_se.time_slice %d", proc->rt_se.time_slice);
//                 current_pcb().time_slice-=1;
//                 self.enqueue_task_rt(current_pcb().priority as usize, proc);
//             }
//         }
// }

//     fn enqueue(&mut self, pcb: &'static mut process_control_block) {
//         let cpu_queue = &mut self.cpu_queue[pcb.cpu_id as usize];
//         cpu_queue.enqueue(pcb);
//     }
// }
