#include <common/kthread.h>
#include <io/block/block_io_scheduler.h>
#include <sched/sched.h>
#include <smp/smp.h>
/**
 * @brief 初始化io调度器
 */
void block_io_scheduler_init()
{
    // 使用rust中的函数进行初始化
    block_io_scheduler_init_rust();
    struct process_control_block *pcb = kthread_run(&block_io_scheduler_address_requests, NULL, "block_io_scheduler", NULL);
    if (smp_get_total_cpu() > 1)
        sched_migrate_process(pcb, 1);
}