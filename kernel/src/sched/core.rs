use core::sync::atomic::compiler_fence;

use crate::{
    arch::asm::{current::current_pcb, ptrace::user_mode},
    arch::{
        context::switch_process,
        interrupt::{cli, sti},
    },
    include::bindings::bindings::smp_get_total_cpu,
    include::bindings::bindings::{
        process_control_block, pt_regs, MAX_CPU_NUM, PF_NEED_MIGRATE, PROC_RUNNING, SCHED_FIFO,
        SCHED_NORMAL, SCHED_RR,
    },
    process::process::process_cpu,
    syscall::SystemError,
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
pub fn loads_balance(pcb: &mut process_control_block) {
    // 对pcb的迁移情况进行调整
    // 获取总的CPU数量
    let cpu_num = unsafe { smp_get_total_cpu() };
    // 获取当前负载最小的CPU的id
    let mut min_loads_cpu_id = pcb.cpu_id;
    let mut min_loads = get_cpu_loads(pcb.cpu_id);
    for cpu_id in 0..cpu_num {
        let tmp_cpu_loads = get_cpu_loads(cpu_id);
        if min_loads - tmp_cpu_loads > 0 {
            min_loads_cpu_id = cpu_id;
            min_loads = tmp_cpu_loads;
        }
    }

    // 将当前pcb迁移到负载最小的CPU
    // 如果当前pcb的PF_NEED_MIGRATE已经置位，则不进行迁移操作
    if (min_loads_cpu_id != pcb.cpu_id) && (pcb.flags & (PF_NEED_MIGRATE as u64)) == 0 {
        // sched_migrate_process(pcb, min_loads_cpu_id as usize);
        pcb.flags |= PF_NEED_MIGRATE as u64;
        pcb.migrate_to = min_loads_cpu_id;
        // kdebug!("set migrating, pcb:{:?}", pcb);
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
    match rt_scheduler.pick_next_task_rt(current_pcb().cpu_id) {
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

/// @brief 将进程加入调度队列
///
/// @param pcb 要被加入队列的pcb
/// @param reset_time 是否重置虚拟运行时间
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_enqueue(pcb: &'static mut process_control_block, mut reset_time: bool) {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    // 调度器不处理running位为0的进程
    if pcb.state & (PROC_RUNNING as u64) == 0 {
        return;
    }
    let cfs_scheduler = __get_cfs_scheduler();
    let rt_scheduler = __get_rt_scheduler();

    // 除了IDLE以外的进程，都进行负载均衡
    if pcb.pid > 0 {
        loads_balance(pcb);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    if (pcb.flags & (PF_NEED_MIGRATE as u64)) != 0 {
        // kdebug!("migrating pcb:{:?}", pcb);
        pcb.flags &= !(PF_NEED_MIGRATE as u64);
        pcb.cpu_id = pcb.migrate_to;
        reset_time = true;
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

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
        return SystemError::EPERM.to_posix_errno() as u64;
    }
    // 根据调度结果统一进行切换
    let pcb = __sched();
    if pcb.is_some() {
        switch_process(current_pcb(), pcb.unwrap());
    }
    sti();
    return 0;
}

#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_set_cpu_idle(cpu_id: usize, pcb: *mut process_control_block) {
    __get_cfs_scheduler().set_cpu_idle(cpu_id, pcb);
}

/// @brief 设置进程需要等待迁移到另一个cpu核心。
/// 当进程被重新加入队列时，将会更新其cpu_id,并加入正确的队列
///
/// @return i32 成功返回0,否则返回posix错误码
#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn sched_migrate_process(
    pcb: &'static mut process_control_block,
    target: usize,
) -> i32 {
    if target > MAX_CPU_NUM.try_into().unwrap() {
        // panic!("sched_migrate_process: target > MAX_CPU_NUM");
        return SystemError::EINVAL.to_posix_errno();
    }

    pcb.flags |= PF_NEED_MIGRATE as u64;
    pcb.migrate_to = target as u32;
    // kdebug!("pid:{} migrate to cpu:{}", pcb.pid, target);
    return 0;
}
