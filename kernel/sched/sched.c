#include "sched.h"
#include <common/kprint.h>
#include <driver/video/video.h>
#include <common/spinlock.h>
#include <sched/cfs.h>

/**
 * @brief 包裹shced_cfs_enqueue(),将PCB加入就绪队列
 *
 * @param pcb
 */
void sched_enqueue(struct process_control_block *pcb)
{
    sched_cfs_enqueue(pcb);
}

/**
 * @brief 包裹sched_cfs(),调度函数
 *
 */
void sched()
{
    kinfo("**************sched  Starting...");
    sched_cfs();
}

void sched_init()
{
    sched_cfs_init();
}