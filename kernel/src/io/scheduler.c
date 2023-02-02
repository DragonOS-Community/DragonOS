#include <common/kthread.h>
#include <io/scheduler.h>
#include <sched/sched.h>
#include <smp/smp.h>
/**
 * @brief 初始化io调度器
 */
void io_scheduler_init()
{
    io_scheduler_init_rust();
    struct process_control_block *pcb = kthread_run(&io_scheduler_address_requests, NULL, "io_scheduler", NULL);
    if (smp_get_total_cpu() > 1)
        sched_migrate_process(pcb, 1);
}