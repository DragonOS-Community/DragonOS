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
#include <asm/irqflags.h>
#include <common/glib.h>
#include <debug/bug.h>

/**
 * @brief 定义自旋锁结构体
 *
 */
typedef struct
{
    int8_t lock; // 1:unlocked 0:locked
} spinlock_t;

extern void __arch_spin_lock(spinlock_t *lock);
extern void __arch_spin_unlock(spinlock_t *lock);

extern void __arch_spin_lock_no_preempt(spinlock_t *lock);
extern void __arch_spin_unlock_no_preempt(spinlock_t *lock);

extern long __arch_spin_trylock(spinlock_t *lock);

/**
 * @brief 自旋锁加锁
 *
 * @param lock
 */
void spin_lock(spinlock_t *lock)
{
    __arch_spin_lock(lock);
}

/**
 * @brief 自旋锁解锁
 *
 * @param lock
 */
void spin_unlock(spinlock_t *lock)
{
    __arch_spin_unlock(lock);
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
    __arch_spin_lock_no_preempt(lock);
}

/**
 * @brief 自旋锁解锁（不改变自旋锁持有计数）
 *
 * @warning 慎用此函数，除非你有十足的把握不会产生自旋锁计数错误
 */
void spin_unlock_no_preempt(spinlock_t *lock)
{
    __arch_spin_unlock_no_preempt(lock);
}

/**
 * @brief 尝试加锁
 *
 * @param lock
 * @return long 锁变量的值（1为成功加锁，0为加锁失败）
 */
long spin_trylock(spinlock_t *lock)
{
    return __arch_spin_trylock(lock);
}

/**
 * @brief 保存中断状态，关闭中断，并自旋锁加锁
 *
 */
#define spin_lock_irqsave(lock, flags)                                                                                 \
    do                                                                                                                 \
    {                                                                                                                  \
        local_irq_save(flags);                                                                                         \
        spin_lock(lock);                                                                                               \
    } while (0)

/**
 * @brief 恢复rflags以及中断状态并解锁自旋锁
 *
 */
#define spin_unlock_irqrestore(lock, flags)                                                                            \
    do                                                                                                                 \
    {                                                                                                                  \
        spin_unlock(lock);                                                                                             \
        local_irq_restore(flags);                                                                                      \
    } while (0)

/**
 * @brief 关闭中断并加锁
 *
 */
#define spin_lock_irq(lock)                                                                                            \
    do                                                                                                                 \
    {                                                                                                                  \
        local_irq_disable();                                                                                           \
        spin_lock(lock);                                                                                               \
    } while (0)

/**
 * @brief 解锁并开启中断
 *
 */
#define spin_unlock_irq(lock)                                                                                          \
    do                                                                                                                 \
    {                                                                                                                  \
        spin_unlock(lock);                                                                                             \
        local_irq_enable();                                                                                            \
    } while (0)

/**
 * @brief 判断自旋锁是否已经加锁
 *
 * @param lock 待判断的自旋锁
 * @return true 已经加锁
 * @return false 尚未加锁
 */
static inline bool spin_is_locked(const spinlock_t *lock)
{
    int x = READ_ONCE(lock->lock);
    return (x == 0) ? true : false;
}

#define assert_spin_locked(lock) BUG_ON(!spin_is_locked(lock))