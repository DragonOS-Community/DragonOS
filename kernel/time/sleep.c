#include "sleep.h"
#include <common/errno.h>
#include <time/timer.h>
#include <process/process.h>
#include <sched/sched.h>
#include <mm/slab.h>
/**
 * @brief nanosleep定时事件到期后，唤醒指定的进程
 *
 * @param pcb 待唤醒的进程的pcb
 */
void nanosleep_handler(void *pcb)
{
    process_wakeup((struct process_control_block *)pcb);
}

/**
 * @brief 休眠指定时间
 *
 * @param rqtp 指定休眠的时间
 * @param rmtp 返回的剩余休眠时间
 * @return int
 */
int nanosleep(const struct timespec *rqtp, struct timespec *rmtp)
{
    int64_t total_ns = rqtp->tv_nsec;
    // kdebug("totalns = %ld", total_ns);
    if (total_ns < 0 || total_ns >= 1000000000)
        return -EINVAL;

    // todo: 对于小于500us的时间，使用spin/rdtsc来进行定时
    if (total_ns < 50000)
        return 0;

    if (total_ns < 500000)
        total_ns = 500000;

    // 增加定时任务
    struct timer_func_list_t *sleep_task = (struct timer_func_list_t *)kmalloc(sizeof(struct timer_func_list_t), 0);
    memset(sleep_task, 0, sizeof(struct timer_func_list_t));

    

    
    timer_func_init_us(sleep_task, &nanosleep_handler, (void *)current_pcb, total_ns / 1000);
    
    timer_func_add(sleep_task);

    current_pcb->state = PROC_INTERRUPTIBLE;
    current_pcb->flags |= PF_NEED_SCHED;
    sched_cfs();

    // todo: 增加信号唤醒的功能后，设置rmtp

    if (rmtp != NULL)
    {
        rmtp->tv_nsec = 0;
        rmtp->tv_sec = 0;
    }

    return 0;
}