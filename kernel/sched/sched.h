#pragma once

#include <common/glib.h>
#include <process/process.h>

// @todo: 用红黑树重写cfs的队列
struct sched_queue_t
{
    long count; // 当前队列中的数量
    long cpu_exec_proc_jiffies; // 进程可执行的时间片数量
    struct process_control_block proc_queue;
};


extern struct sched_queue_t sched_cfs_ready_queue[MAX_CPU_NUM]; // 就绪队列

/**
 * @brief 调度函数
 * 
 */
void sched_cfs();

/**
 * @brief 包裹sched_cfs()，调度函数
 * 
 */
void sched();

/**
 * @brief 将PCB加入就绪队列
 *
 * @param pcb
 */
void sched_cfs_enqueue(struct process_control_block *pcb);

/**
 * @brief 包裹sched_enqueue(),将PCB加入就绪队列
 * 
 * @param pcb
 */
void sched_enqueue(struct process_control_block *pcb);

/**
 * @brief 从就绪队列中取出PCB
 *
 * @return struct process_control_block*
 */
struct process_control_block *sched_cfs_dequeue();

/**
 * @brief 初始化进程调度器
 * 
 */
void sched_init();

/**
 * @brief 当时钟中断到达时，更新时间片
 * 
 */
void sched_update_jiffies();

