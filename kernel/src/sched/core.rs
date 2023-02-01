use core::sync::atomic::compiler_fence;

use crate::{
    arch::asm::{current::current_pcb, ptrace::user_mode},
    arch::{context::switch_process, interrupt::{cli, sti}},
    include::bindings::bindings::{
        process_control_block, pt_regs, EPERM, PROC_RUNNING, SCHED_FIFO, SCHED_NORMAL, SCHED_RR,
    },
    kdebug,
    process::process::process_cpu,
};

use super::cfs::{sched_cfs_init, SchedulerCFS, __get_cfs_scheduler};
use super::rt::{sched_rt_init, SchedulerRT, __get_rt_scheduler};

/// @brief 获取指定的cpu上正在执行的进程的pcb
#[inline]
pub fn cpu_executing(cpu_id: u32) -> &'static mut process_control_block {
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
    fn sched(&mut self) -> Option<&'static mut process_control_block>;

    /// @brief 将pcb加入这个调度器的调度队列
    fn enqueue(&mut self, pcb: &'static mut process_control_block);
}

fn __sched() -> Option<&'static mut process_control_block> {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let cfs_scheduler: &mut SchedulerCFS = __get_cfs_scheduler();
    let rt_scheduler: &mut SchedulerRT = __get_rt_scheduler();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let next: &'static mut process_control_block;
    match rt_scheduler.pick_next_task_rt() {
        Some(p) => {
            next = p;
            // kdebug!("next pcb is {}",next.pid);
            // rt_scheduler.enqueue_task_rt(next.priority as usize, next);
            sched_enqueue(next, false);
            return rt_scheduler.sched();
        }
        None => {
            return cfs_scheduler.sched();
        }
    }
}

/// @brief 将进程加入调度队列
///
/// @param pcb 要被加入队列的pcb
/// @param reset_time 是否重置虚拟运行时间
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_enqueue(pcb: &'static mut process_control_block, reset_time: bool) {
    // 调度器不处理running位为0的进程
    if pcb.state & (PROC_RUNNING as u64) == 0 {
        return;
    }
    let cfs_scheduler = __get_cfs_scheduler();
    let rt_scheduler = __get_rt_scheduler();
    if pcb.policy == SCHED_NORMAL {
        if reset_time {
            cfs_scheduler.enqueue_reset_vruntime(pcb);
        } else {
            cfs_scheduler.enqueue(pcb);
        }
    } else if pcb.policy == SCHED_FIFO || pcb.policy == SCHED_RR {
        rt_scheduler.enqueue(pcb);
    } else {
        panic!("This policy is not supported at this time");
    }
}

/// @brief 初始化进程调度器模块
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_init() {
    unsafe {
        sched_cfs_init();
        sched_rt_init();
    }
}

/// @brief 当时钟中断到达时，更新时间片
/// 请注意，该函数只能被时钟中断处理程序调用
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_update_jiffies() {
    match current_pcb().policy {
        SCHED_NORMAL => {
            __get_cfs_scheduler().timer_update_jiffies();
        }
        SCHED_FIFO | SCHED_RR => {
            current_pcb().rt_time_slice -= 1;
        }
        _ => {
            todo!()
        }
    }
}

/// @brief 让系统立即运行调度器的系统调用
/// 请注意，该系统调用不能由ring3的程序发起
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sys_sched(regs: &'static mut pt_regs) -> u64 {
    cli();
    // 进行权限校验，拒绝用户态发起调度
    if user_mode(regs) {
        return (-(EPERM as i64)) as u64;
    }
    // 根据调度结果统一进行切换
    let pcb = __sched();
    if pcb.is_some() {
        switch_process(current_pcb(), pcb.unwrap());
    }
    sti();
    return 0;
}
