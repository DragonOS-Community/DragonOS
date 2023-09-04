use core::sync::atomic::compiler_fence;

use alloc::{sync::Arc, vec::Vec};

use crate::{
    arch::cpu::current_cpu_id,
    include::bindings::bindings::process_control_block,
    include::bindings::bindings::smp_get_total_cpu,
    kinfo,
    mm::percpu::PerCpu,
    process::{AtomicPid, Pid, ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState},
};

use super::rt::{sched_rt_init, SchedulerRT, __get_rt_scheduler};
use super::{
    cfs::{sched_cfs_init, SchedulerCFS, __get_cfs_scheduler},
    SchedPolicy,
};

lazy_static! {
    /// 记录每个cpu上正在执行的进程的pid
    pub static ref CPU_EXECUTING: Vec<AtomicPid> = {
        let mut v = Vec::new();
        for _ in 0..PerCpu::MAX_CPU_NUM {
            v.push(AtomicPid::new(Pid::new(0)));
        }
        v
    };
}

// 获取某个cpu的负载情况，返回当前负载，cpu_id 是获取负载的cpu的id
// TODO:将获取负载情况调整为最近一段时间运行进程的数量
pub fn get_cpu_loads(cpu_id: u32) -> u32 {
    let cfs_scheduler = __get_cfs_scheduler();
    let rt_scheduler = __get_rt_scheduler();
    let len_cfs = cfs_scheduler.get_cfs_queue_len(cpu_id);
    let len_rt = rt_scheduler.rt_queue_len(cpu_id);
    // let load_rt = rt_scheduler.get_load_list_len(cpu_id);
    // kdebug!("this cpu_id {} is load rt {}", cpu_id, load_rt);

    return (len_rt + len_cfs) as u32;
}
// 负载均衡
pub fn loads_balance(pcb: Arc<ProcessControlBlock>) {
    // 对pcb的迁移情况进行调整
    // 获取总的CPU数量
    let cpu_num = unsafe { smp_get_total_cpu() };
    // 获取当前负载最小的CPU的id
    let mut min_loads_cpu_id = current_cpu_id();
    let mut min_loads = get_cpu_loads(current_cpu_id());
    for cpu_id in 0..cpu_num {
        let tmp_cpu_loads = get_cpu_loads(cpu_id);
        if min_loads - tmp_cpu_loads > 0 {
            min_loads_cpu_id = cpu_id;
            min_loads = tmp_cpu_loads;
        }
    }

    let pcb_cpu = pcb.sched_info().on_cpu();
    // 将当前pcb迁移到负载最小的CPU
    // 如果当前pcb的PF_NEED_MIGRATE已经置位，则不进行迁移操作
    if pcb_cpu.is_none()
        || (min_loads_cpu_id != pcb_cpu.unwrap()
            && !pcb.flags().contains(ProcessFlags::NEED_MIGRATE))
    {
        pcb.flags().insert(ProcessFlags::NEED_MIGRATE);
        pcb.sched_info().set_migrate_to(Some(min_loads_cpu_id));
        // kdebug!("set migrating, pcb:{:?}", pcb);
    }
}
/// @brief 具体的调度器应当实现的trait
pub trait Scheduler {
    /// @brief 使用该调度器发起调度的时候，要调用的函数
    fn sched(&mut self) -> Option<Arc<ProcessControlBlock>>;

    /// @brief 将pcb加入这个调度器的调度队列
    fn enqueue(&mut self, pcb: Arc<ProcessControlBlock>);
}

pub fn do_sched() -> Option<Arc<ProcessControlBlock>> {
    // 当前进程持有锁，不切换，避免死锁
    if ProcessManager::current_pcb().preempt_count() != 0 {
        return None;
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let cfs_scheduler: &mut SchedulerCFS = __get_cfs_scheduler();
    let rt_scheduler: &mut SchedulerRT = __get_rt_scheduler();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let next: Arc<ProcessControlBlock>;
    match rt_scheduler.pick_next_task_rt(current_cpu_id()) {
        Some(p) => {
            next = p;
            // kdebug!("next pcb is {}",next.pid);
            // 将pick的进程放回原处
            rt_scheduler.enqueue_front(next);

            return rt_scheduler.sched();
        }
        None => {
            return cfs_scheduler.sched();
        }
    }
}

// c版本代码
// /// @brief 将进程加入调度队列
// ///
// /// @param pcb 要被加入队列的pcb
// /// @param reset_time 是否重置虚拟运行时间
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_enqueue_old(pcb: &'static mut process_control_block, mut reset_time: bool) {
    panic!("derived method")
}

/// @brief 将进程加入调度队列
///
/// @param pcb 要被加入队列的pcb
/// @param reset_time 是否重置虚拟运行时间
pub fn sched_enqueue(pcb: Arc<ProcessControlBlock>, mut reset_time: bool) {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    if pcb.sched_info().state() != ProcessState::Runnable {
        return;
    }
    let cfs_scheduler = __get_cfs_scheduler();
    let rt_scheduler = __get_rt_scheduler();
    // 除了IDLE以外的进程，都进行负载均衡
    if pcb.pid().into() > 0 {
        loads_balance(pcb.clone());
    }

    if pcb.flags().contains(ProcessFlags::NEED_MIGRATE) {
        // kdebug!("migrating pcb:{:?}", pcb);
        pcb.flags().remove(ProcessFlags::NEED_MIGRATE);
        pcb.sched_info().set_on_cpu(pcb.sched_info().migrate_to());
        reset_time = true;
    }

    assert!(pcb.sched_info().on_cpu().is_some());

    match pcb.sched_info().policy() {
        SchedPolicy::CFS => {
            if reset_time {
                cfs_scheduler.enqueue_reset_vruntime(pcb.clone());
            } else {
                cfs_scheduler.enqueue(pcb.clone());
            }
        }
        SchedPolicy::FIFO | SchedPolicy::RR => rt_scheduler.enqueue(pcb.clone()),
    }
}

/// @brief 初始化进程调度器模块
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_init() {
    kinfo!("Initializing schedulers...");
    unsafe {
        sched_cfs_init();
        sched_rt_init();
    }
    kinfo!("Schedulers initialized");
}

/// @brief 当时钟中断到达时，更新时间片
/// 请注意，该函数只能被时钟中断处理程序调用
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_update_jiffies() {
    let policy = ProcessManager::current_pcb().sched_info().policy();
    match policy {
        SchedPolicy::CFS => {
            __get_cfs_scheduler().timer_update_jiffies();
        }
        SchedPolicy::FIFO | SchedPolicy::RR => {
            __get_rt_scheduler().timer_update_jiffies();
        }
    }
}
