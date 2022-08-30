#pragma once
#include <common/glib.h>

/**
 * @brief 信号量的等待队列
 *
 */
typedef struct
{
    struct List wait_list;
    struct process_control_block *pcb;
} wait_queue_node_t;

/**
 * @brief 初始化等待队列
 *
 * @param wait_queue 等待队列
 * @param pcb pcb
 */
void wait_queue_init(wait_queue_node_t *wait_queue, struct process_control_block *pcb);

/**
 * @brief 在等待队列上进行等待
 * 
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_on(wait_queue_node_t * wait_queue_head);

/**
 * @brief 在等待队列上进行等待,同时释放自旋锁
 * 
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_on_unlock(wait_queue_node_t *wait_queue_head,
                                void *lock);
/**
 * @brief 在等待队列上进行等待(允许中断)
 * 
 * @param wait_queue_head 队列头指针
 */
void wait_queue_sleep_on_interriptible(wait_queue_node_t * wait_queue_head);

/**
 * @brief 唤醒在等待队列的头部的进程
 * 
 * @param wait_queue_head 队列头
 * @param state 要唤醒的进程的状态
 */
void wait_queue_wakeup(wait_queue_node_t * wait_queue_head, int64_t state);