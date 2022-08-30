/**
 * @file semaphore.h
 * @author fslngjin (lonjin@RinGoTek.cn)
 * @brief 信号量
 * @version 0.1
 * @date 2022-04-12
 *
 * @copyright Copyright (c) 2022
 *
 */

#pragma once
#include <common/atomic.h>

#include <common/wait_queue.h>

/**
 * @brief 信号量的结构体
 *
 */
typedef struct
{
    atomic_t counter;
    wait_queue_node_t wait_queue;
} semaphore_t;


/**
 * @brief 初始化信号量
 *
 * @param sema 信号量对象
 * @param count 信号量的初始值
 */
static __always_inline void semaphore_init(semaphore_t *sema, ul count)
{
    atomic_set(&sema->counter, count);
    wait_queue_init(&sema->wait_queue, NULL);
}

/**
 * @brief 信号量down
 *
 * @param sema
 */
void semaphore_down(semaphore_t *sema);

void semaphore_up(semaphore_t *sema);