#include "rt.h"

struct sched_queue_rt sched_rt_ready_queue[MAX_CPU_NUM]; // 就绪队列

/**
 * @brief 调度函数
 *
 */
void sched_rt()
{
}

/**
 * @brief 将PCB加入就绪队列
 *
 * @param pcb
 */
void sched_rt_enqueue(struct process_control_block *pcb)
{
}

/**
 * @brief 从就绪队列中取出PCB
 *
 * @return struct process_control_block*
 */
struct process_control_block *sched_rt_dequeue()
{
}
/**
 * @brief 初始化RT进程调度器
 *
 */
void sched_rt_init()
{

    memset(&sched_rt_ready_queue, 0, sizeof(struct sched_queue_t) * MAX_CPU_NUM);
    for (int i = 0; i < MAX_CPU_NUM; ++i)
    {

        list_init(&sched_rt_ready_queue[i].proc_queue.list);
        sched_rt_ready_queue[i].count = 1; // 因为存在IDLE进程，因此为1
        sched_rt_ready_queue[i].cpu_exec_proc_jiffies = 5;
        sched_rt_ready_queue[i].proc_queue.virtual_runtime = 0x7fffffffffffffff;
    }
}
