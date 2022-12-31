#include "sleep.h"
#include <common/errno.h>
#include <time/timer.h>
#include <process/process.h>
#include <sched/sched.h>
#include <mm/slab.h>
#include <common/cpu.h>
#include <common/glib.h>

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

    if (rqtp->tv_nsec < 0 || rqtp->tv_nsec >= 1000000000)
        return -EINVAL;


    // 对于小于500us的时间，使用spin/rdtsc来进行定时
    if (rqtp->tv_nsec < 500000)
    {
        uint64_t expired_tsc = rdtsc() + (((uint64_t)rqtp->tv_nsec) * Cpu_tsc_freq) / 1000000000;
        while (rdtsc() < expired_tsc)
            ;

        if (rmtp != NULL)
        {
            rmtp->tv_nsec = 0;
            rmtp->tv_sec = 0;
        }
        return 0;
    }

    // 增加定时任务
    struct timer_func_list_t *sleep_task = (struct timer_func_list_t *)kmalloc(sizeof(struct timer_func_list_t), 0);
    memset(sleep_task, 0, sizeof(struct timer_func_list_t));

    timer_func_init_us(sleep_task, &nanosleep_handler, (void *)current_pcb, rqtp->tv_nsec / 1000);

    timer_func_add(sleep_task);

    current_pcb->state = PROC_INTERRUPTIBLE;
    current_pcb->flags |= PF_NEED_SCHED;
    sched();

    // todo: 增加信号唤醒的功能后，设置rmtp

    if (rmtp != NULL)
    {
        rmtp->tv_nsec = 0;
        rmtp->tv_sec = 0;
    }

    return 0;
}

/**
 * @brief 睡眠指定时间
 *
 * @param usec 微秒
 * @return int
 */
int usleep(useconds_t usec)
{
    struct timespec ts = {
        tv_sec : (long int)(usec / 1000000),
        tv_nsec : (long int)(usec % 1000000) * 1000UL
    };

    return nanosleep(&ts, NULL);
}
