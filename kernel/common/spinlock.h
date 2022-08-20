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
    int8_t lock; // 1:unlocked 0:locked
}spinlock_t;


/**
 * @brief 自旋锁加锁
 *
 * @param lock
 */
void spin_lock(spinlock_t *lock)
{
    __asm__ __volatile__("1:    \n\t"
                         "lock decb %0   \n\t" // 尝试-1
                         "jns 3f    \n\t"      // 加锁成功，跳转到步骤3
                         "2:    \n\t"          // 加锁失败，稍后再试
                         "pause \n\t"
                         "cmpb $0, %0   \n\t"
                         "jle   2b  \n\t" // 若锁被占用，则继续重试
                         "jmp 1b    \n\t" // 尝试加锁
                         "3:"
                         : "=m"(lock->lock)::"memory");
    preempt_disable();
}

/**
 * @brief 自旋锁解锁
 *
 * @param lock
 */
void spin_unlock(spinlock_t *lock)
{
    preempt_enable();
    __asm__ __volatile__("movb $1, %0   \n\t"
                         : "=m"(lock->lock)::"memory");
}

/**
 * @brief 初始化自旋锁
 *
 * @param lock
 */
void spin_init(spinlock_t *lock)
{
    barrier();
    lock->lock = 1;
    barrier();
}

/**
 * @brief 自旋锁加锁（不改变自旋锁持有计数）
 *
 * @warning 慎用此函数，除非你有十足的把握不会产生自旋锁计数错误
 */
void spin_lock_no_preempt(spinlock_t *lock)
{
    __asm__ __volatile__("1:    \n\t"
                         "lock decb %0   \n\t" // 尝试-1
                         "jns 3f    \n\t"      // 加锁成功，跳转到步骤3
                         "2:    \n\t"          // 加锁失败，稍后再试
                         "pause \n\t"
                         "cmpb $0, %0   \n\t"
                         "jle   2b  \n\t" // 若锁被占用，则继续重试
                         "jmp 1b    \n\t" // 尝试加锁
                         "3:"
                         : "=m"(lock->lock)::"memory");
}
/**
 * @brief 自旋锁解锁（不改变自旋锁持有计数）
 *
 * @warning 慎用此函数，除非你有十足的把握不会产生自旋锁计数错误
 */
void spin_unlock_no_preempt(spinlock_t *lock)
{
    __asm__ __volatile__("movb $1, %0   \n\t"
                         : "=m"(lock->lock)::"memory");
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
    asm volatile("lock xchg %%bx, %1  \n\t" // 确保只有1个进程能得到锁
                 : "=q"(tmp_val), "=m"(lock->lock)
                 : "b"(0)
                 : "memory");
    if (!tmp_val)
        preempt_enable();
    return tmp_val;
}

// 保存当前rflags的值到变量x内并关闭中断
#define local_irq_save(x) __asm__ __volatile__("pushfq ; popq %0 ; cli" \
                                               : "=g"(x)::"memory")
// 恢复先前保存的rflags的值x
#define local_irq_restore(x) __asm__ __volatile__("pushq %0 ; popfq" ::"g"(x) \
                                                  : "memory")
#define local_irq_disable() cli();
#define local_irq_enable() sti();

/**
 * @brief 保存中断状态，关闭中断，并自旋锁加锁
 *
 */
#define spin_lock_irqsave(lock, flags) \
    do                                 \
    {                                  \
        local_irq_save(flags);         \
        spin_lock(lock);               \
    } while (0)

/**
 * @brief 恢复rflags以及中断状态并解锁自旋锁
 *
 */
#define spin_unlock_irqrestore(lock, flags) \
    do                                      \
    {                                       \
        spin_unlock(lock);                  \
        local_irq_restore(flags);           \
    } while (0)

/**
 * @brief 关闭中断并加锁
 *
 */
#define spin_lock_irq(lock)  \
    do                       \
    {                        \
        local_irq_disable(); \
        spin_lock(lock);     \
    } while (0)

/**
 * @brief 解锁并开启中断
 *
 */
#define spin_unlock_irq(lock) \
    do                        \
    {                         \
        spin_unlock(lock);    \
        local_irq_enable();   \
    } while (0)
