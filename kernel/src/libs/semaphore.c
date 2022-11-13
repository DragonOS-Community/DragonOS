#include <common/semaphore.h>
#include <sched/sched.h>
#include <process/process.h>


void semaphore_down(semaphore_t *sema)
{
    if (atomic_read(&sema->counter) > 0) // 信号量大于0，资源充足
        atomic_dec(&sema->counter);
    else // 资源不足，进程休眠
    {
        // 将当前进程加入信号量的等待队列
        wait_queue_node_t wait;
        wait_queue_init(&wait, current_pcb);

        current_pcb->state = PROC_UNINTERRUPTIBLE;

        list_append(&sema->wait_queue.wait_list, &wait.wait_list);

        // 执行调度
        sched();
    }
}

void semaphore_up(semaphore_t *sema)
{
    if (list_empty(&sema->wait_queue.wait_list)) // 没有进程在等待资源
    {
        atomic_inc(&sema->counter);
    }
    else    // 有进程在等待资源，唤醒进程
    {

        wait_queue_node_t *wq = container_of(list_next(&sema->wait_queue.wait_list), wait_queue_node_t, wait_list);
        list_del(&wq->wait_list);

        wq->pcb->state = PROC_RUNNING;
        sched_enqueue(wq->pcb);

        // 当前进程缺少需要的资源，立即标为需要被调度
        current_pcb->flags |= PF_NEED_SCHED;
    }
};