#include "cfs.h"
// #include "sched.h"
// #include <common/kprint.h>
// #include <driver/video/video.h>
// #include <common/spinlock.h>
/**
 * @brief 调度函数
 *
 */
void sched_cfs()
{

    cli();

    current_pcb->flags &= ~PF_NEED_SCHED;
    struct process_control_block *proc = sched_cfs_dequeue();
    // kdebug("sched_cfs_ready_queue[proc_current_cpu_id].count = %d", sched_cfs_ready_queue[proc_current_cpu_id].count);
    if (current_pcb->virtual_runtime >= proc->virtual_runtime || current_pcb->state != PROC_RUNNING) // 当前进程运行时间大于了下一进程的运行时间，进行切换
    {

        if (current_pcb->state == PROC_RUNNING) // 本次切换由于时间片到期引发，则再次加入就绪队列，否则交由其它功能模块进行管理
            sched_cfs_enqueue(current_pcb);
        // kdebug("proc->pid=%d, count=%d", proc->pid, sched_cfs_ready_queue[proc_current_cpu_id].count);
        if (sched_cfs_ready_queue[proc_current_cpu_id].cpu_exec_proc_jiffies <= 0)
        {
            switch (proc->priority)
            {
            case 0:
            case 1:
                sched_cfs_ready_queue[proc_current_cpu_id].cpu_exec_proc_jiffies = 4 / sched_cfs_ready_queue[proc_current_cpu_id].count;
                break;
            case 2:
            default:
                sched_cfs_ready_queue[proc_current_cpu_id].cpu_exec_proc_jiffies = (4 / sched_cfs_ready_queue[proc_current_cpu_id].count) << 2;
                break;
            }
        }

        // if (proc->pid == 0)
        // {
        //     kdebug("switch to pid0, current pid%ld, vrt=%ld      pid0 vrt=%ld", current_pcb->pid, current_pcb->virtual_runtime, proc->virtual_runtime);
        //     if(current_pcb->state != PROC_RUNNING)
        //         kdebug("current_pcb->state!=PROC_RUNNING");
        // }

        process_switch_mm(proc);

        switch_proc(current_pcb, proc);
    }
    else // 不进行切换
    {
        // kdebug("not switch.");
        sched_cfs_enqueue(proc);

        if (sched_cfs_ready_queue[proc_current_cpu_id].cpu_exec_proc_jiffies <= 0)
        {
            switch (proc->priority)
            {
            case 0:
            case 1:
                sched_cfs_ready_queue[proc_current_cpu_id].cpu_exec_proc_jiffies = 4 / sched_cfs_ready_queue[proc_current_cpu_id].count;
                // sched_cfs_ready_queue.cpu_exec_proc_jiffies = 5;
                break;
            case 2:
            default:
                // sched_cfs_ready_queue.cpu_exec_proc_jiffies = 5;

                sched_cfs_ready_queue[proc_current_cpu_id].cpu_exec_proc_jiffies = (4 / sched_cfs_ready_queue[proc_current_cpu_id].count) << 2;
                break;
            }
        }
    }

    sti();
}