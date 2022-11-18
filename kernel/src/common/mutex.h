#pragma once

#include <common/atomic.h>
#include <common/spinlock.h>
#include <common/glib.h>
#include <process/proc-types.h>

/**
 * @brief Mutex - 互斥锁
 *
 * - 同一时间只有1个任务可以持有mutex
 * - 不允许递归地加锁、解锁
 * - 只允许通过mutex的api来操作mutex
 * - 在硬中断、软中断中不能使用mutex
 */
typedef struct
{

    atomic_t count; // 锁计数。1->已解锁。 0->已上锁,且有可能存在等待者
    spinlock_t wait_lock;   // mutex操作锁，用于对mutex的list的操作进行加锁
    struct List wait_list;  // Mutex的等待队列
} mutex_t;

/**
 * @brief 在mutex上的等待者的结构体
 *
 */
struct mutex_waiter_t
{
    struct List list;
    struct process_control_block *pcb;
};

/**
 * @brief 初始化互斥量
 *
 * @param lock mutex结构体
 */
void mutex_init(mutex_t *lock);

/**
 * @brief 对互斥量加锁
 *
 * @param lock mutex结构体
 */
void mutex_lock(mutex_t *lock);

/**
 * @brief 对互斥量解锁
 *
 * @param lock mutex结构体
 */
void mutex_unlock(mutex_t *lock);

/**
 * @brief 尝试对互斥量加锁
 *
 * @param lock mutex结构体
 *
 * @return 成功加锁->1, 加锁失败->0
 */
int mutex_trylock(mutex_t *lock);

/**
 * @brief 判断mutex是否已被加锁
 *
 * @return 已加锁->1, 未加锁->0
 */
#define mutex_is_locked(lock) ((atomic_read(&(lock)->count) == 1) ? 0 : 1)
