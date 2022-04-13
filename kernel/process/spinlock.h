/**
 * @file spinlock.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief 自旋锁
 * @version 0.1
 * @date 2022-04-07
 *
 * @copyright Copyright (c) 2022
 *
 */
#pragma once
#include <common/glib.h>
#include <process/preempt.h>

/**
 * @brief 定义自旋锁结构体
 *
 */
typedef struct
{
    __volatile__ char lock; // 1:unlocked 0:locked
} spinlock_t;

/**
 * @brief 初始化自旋锁
 *
 * @param lock
 */
void spin_init(spinlock_t *lock)
{
    lock->lock = 1;
}

/**
 * @brief 自旋锁加锁
 *
 * @param lock
 */
void spin_lock(spinlock_t *lock)
{
    preempt_disable();
    __asm__ __volatile__("1:    \n\t"
                         "lock decq %0   \n\t" // 尝试-1
                         "jns 3f    \n\t"      // 加锁成功，跳转到步骤3
                         "2:    \n\t"          // 加锁失败，稍后再试
                         "pause \n\t"
                         "cmpq $0, %0   \n\t"
                         "jle   2b  \n\t" // 若锁被占用，则继续重试
                         "jmp 1b    \n\t" // 尝试加锁
                         "3:"
                         : "=m"(lock->lock)::"memory");
}

void spin_unlock(spinlock_t *lock)
{
    __asm__ __volatile__("movq $1, %0   \n\t"
                         : "=m"(lock->lock)::"memory");
    preempt_enable();
}

/**
 * @brief 尝试加锁
 *
 * @param lock
 * @return long 锁变量的值（1为成功加锁，0为加锁失败）
 */
long spin_trylock(spinlock_t *lock)
{
    uint64_t tmp_val = 0;
    preempt_disable();
    // 交换tmp_val和lock的值，若tmp_val==1则证明加锁成功
    asm volatile("lock xchgq %0, %1  \n\t" // 确保只有1个进程能得到锁
                 : "=q"(tmp_val), "=m"(lock->lock)
                 : "0"(0)
                 : "memory");
    if (!tmp_val)
        preempt_enable();
    return tmp_val;
}