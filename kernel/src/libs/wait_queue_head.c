#include <common/wait_queue.h>
#include <process/process.h>
#include <sched/sched.h>

/**
 * @brief 初始化等待队列
 *
 * @param wait_queue 等待队列
 * @param pcb pcb
 */
void wait_queue_head_init(wait_queue_head_t *wait_queue)
{
    list_init(&wait_queue->wait_list);
    spin_init(&wait_queue->lock);
}

/**
 * @brief 在等待队列上进行等待, 但是你需要确保wait已经被init, 同时wakeup只能使用wake_up_on_stack函数。
 *
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_with_node(wait_queue_head_t *q, wait_queue_node_t *wait)
{
    BUG_ON(wait->pcb == NULL);

    wait->pcb->state = PROC_UNINTERRUPTIBLE;
    list_append(&q->wait_list, &wait->wait_list);

    sched();
}

/**
 * @brief 在等待队列上进行等待，同时释放自旋锁, 但是你需要确保wait已经被init, 同时wakeup只能使用wake_up_on_stack函数。
 *
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_with_node_unlock(wait_queue_head_t *q, wait_queue_node_t *wait, void *lock)
{
    BUG_ON(wait->pcb == NULL);

    wait->pcb->state = PROC_UNINTERRUPTIBLE;
    list_append(&q->wait_list, &wait->wait_list);
    spin_unlock((spinlock_t *)lock);

    sched();
}

/**
 * @brief 在等待队列上进行等待(允许中断), 但是你需要确保wait已经被init, 同时wakeup只能使用wake_up_on_stack函数。
 *
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_with_node_interriptible(wait_queue_head_t *q, wait_queue_node_t *wait)
{
    BUG_ON(wait->pcb == NULL);

    wait->pcb->state = PROC_INTERRUPTIBLE;
    list_append(&q->wait_list, &wait->wait_list);

    sched();
}

/**
 * @brief 唤醒在等待队列的头部的进程, 但是不会free掉这个节点的空间(默认这个节点在栈上创建)
 *
 * @param wait_queue_head
 * @param state
 */
void wait_queue_wakeup_on_stack(wait_queue_head_t *q, int64_t state)
{
    if (list_empty(&q->wait_list))
        return;

    wait_queue_node_t *wait = container_of(list_next(&q->wait_list), wait_queue_node_t, wait_list);

    // 符合唤醒条件
    if (wait->pcb->state & state)
    {
        list_del_init(&wait->wait_list);
        process_wakeup(wait->pcb);
    }
}