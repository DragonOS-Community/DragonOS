#include <common/spinlock.h>
#include <common/wait_queue.h>
#include <mm/slab.h>
#include <process/process.h>
#include <sched/sched.h>

/**
 * @brief 初始化等待队列
 *
 * @param wait_queue 等待队列
 * @param pcb pcb
 */
void wait_queue_init(wait_queue_node_t *wait_queue, struct process_control_block *pcb)
{
    list_init(&wait_queue->wait_list);
    wait_queue->pcb = pcb;
}

/**
 * @brief 在等待队列上进行等待
 *
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_on(wait_queue_node_t *wait_queue_head)
{
    wait_queue_node_t *wait = (wait_queue_node_t *)kzalloc(sizeof(wait_queue_node_t), 0);
    wait_queue_init(wait, current_pcb);
    current_pcb->state = PROC_UNINTERRUPTIBLE;
    list_append(&wait_queue_head->wait_list, &wait->wait_list);

    sched();
}

/**
 * @brief 在等待队列上进行等待，同时释放自旋锁
 *
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_on_unlock(wait_queue_node_t *wait_queue_head,
                                void *lock)
{
    wait_queue_node_t *wait = (wait_queue_node_t *)kzalloc(sizeof(wait_queue_node_t), 0);
    wait_queue_init(wait, current_pcb);
    current_pcb->state = PROC_UNINTERRUPTIBLE;
    list_append(&wait_queue_head->wait_list, &wait->wait_list);
    spin_unlock((spinlock_t *)lock);
    sched();
}

/**
 * @brief 在等待队列上进行等待(允许中断)
 *
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_on_interriptible(wait_queue_node_t *wait_queue_head)
{
    wait_queue_node_t *wait = (wait_queue_node_t *)kzalloc(sizeof(wait_queue_node_t), 0);
    wait_queue_init(wait, current_pcb);
    current_pcb->state = PROC_INTERRUPTIBLE;
    list_append(&wait_queue_head->wait_list, &wait->wait_list);

    sched();
}

/**
 * @brief 唤醒在等待队列的头部的进程
 *
 * @param wait_queue_head
 * @param state
 */
void wait_queue_wakeup(wait_queue_node_t *wait_queue_head, int64_t state)
{
    if (list_empty(&wait_queue_head->wait_list))
        return;
    wait_queue_node_t *wait = container_of(list_next(&wait_queue_head->wait_list), wait_queue_node_t, wait_list);

    // 符合唤醒条件
    if (wait->pcb->state & state)
    {
        list_del(&wait->wait_list);
        process_wakeup(wait->pcb);
        kfree(wait);
    }
}