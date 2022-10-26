
#pragma once

#include <common/glib.h>
#include <process/process.h>

struct sched_queue_rt
{

    long count;                 // 当前队列中的数量
    long cpu_exec_proc_jiffies; // 进程可执行的时间片数量
    struct process_control_block proc_queue;
};

extern struct sched_queue_rt sched_rt_ready_queue[MAX_CPU_NUM]; // 就绪队列

/**
 * @brief 调度函数
 *
 */
void sched_rt();

/**
 * @brief 将PCB加入就绪队列
 *
 * @param pcb
 */
void sched_rt_enqueue(struct process_control_block *pcb);

/**
 * @brief 从就绪队列中取出PCB
 *
 * @return struct process_control_block*
 */
struct process_control_block *sched_rt_dequeue();
/**
 * @brief 初始化CFS进程调度器
 *
 */
void sched_rt_init();
