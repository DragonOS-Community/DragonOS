use core::sync::atomic::compiler_fence;

use crate::{
    arch::asm::{current::current_pcb, ptrace::user_mode},
    arch::context::switch_process,
    include::bindings::bindings::{process_control_block, pt_regs, EPERM, SCHED_NORMAL,SCHED_FIFO,SCHED_RR,PROC_RUNNING,},
    process::process::process_cpu,
    kdebug,
};

use super::cfs::{sched_cfs_init, SchedulerCFS, __get_cfs_scheduler};
use super::rt::{sched_RT_init,SchedulerRT,__get_rt_scheduler};

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

fn __sched() ->Option<&'static mut process_control_block>{
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let cfs_scheduler: &mut SchedulerCFS = __get_cfs_scheduler();
    let rt_scheduler: &mut SchedulerRT = __get_rt_scheduler();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    // let next = rt_scheduler.pick_next_task_rt();
    // let next: &'static mut process_control_block =rt_scheduler.pick_next_task_rt().expect("No RT process found");

    let mut next: &'static mut process_control_block;
    kdebug!("__sched:currpcb pid is {}",current_pcb().pid);
    kdebug!("__sched:currpcb policy is {}",current_pcb().policy);
    if current_pcb().policy==SCHED_RR{
        kdebug!("__sched:currpcb time_slice is {}",current_pcb().time_slice);
        kdebug!("__sched:currpcb state is {}",current_pcb().state);
    }
    match rt_scheduler.pick_next_task_rt() {
        Some(p) => {
            next = p;
            kdebug!("next :: policy {}",next.policy);
            kdebug!("next :: priority {}",next.priority as usize);
            kdebug!("next :: state {}",next.state);
            rt_scheduler.enqueue_task_rt(next.priority as usize,next);
            kdebug!("sched:sched_rt is begin");

            return rt_scheduler.sched();
        },
        None => {
            kdebug!("next is null");
            if current_pcb().policy==SCHED_NORMAL||(current_pcb().state & (PROC_RUNNING as u64) != 0){
            // if current_pcb().policy==SCHED_NORMAL||current_pcb().policy==SCHED_RR{
                kdebug!("sched:sched_cfs is begin");

                return cfs_scheduler.sched();
            }
        },
    }
    return None;
}

/// @brief 将进程加入调度队列
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_enqueue(pcb: &'static mut process_control_block) {
    let cfs_scheduler = __get_cfs_scheduler();
    let rt_scheduler = __get_rt_scheduler();
    if pcb.policy==SCHED_NORMAL{
        cfs_scheduler.enqueue(pcb);
    }
    else{
        rt_scheduler.enqueue(pcb);
    }
}

/// @brief 初始化进程调度器模块
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_init() {
    unsafe {
        sched_cfs_init();
        sched_RT_init();
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
            // kdebug!("sched_update_jiffies RR");
            current_pcb().time_slice-=1;
        }
        _ => {
            kdebug!("todo pid {}",current_pcb().pid);
            kdebug!("todo policy {}",current_pcb().policy);
            todo!()
        }
    }
}

/// @brief 让系统立即运行调度器的系统调用
/// 请注意，该系统调用不能由ring3的程序发起
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sys_sched(regs: &'static mut pt_regs) -> u64 {
    // 进行权限校验，拒绝用户态发起调度
    if user_mode(regs) {
        return (-(EPERM as i64)) as u64;
    }
    let pcb=__sched();
    if pcb.is_some(){
        switch_process(current_pcb(), pcb.unwrap());
    }
    0
}
