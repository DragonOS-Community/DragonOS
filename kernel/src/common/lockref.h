#pragma once

#include <common/sys/types.h>
#include <common/spinlock.h>

#if ARCH(X86_64)
// 仅在x64架构下启用cmpxchg
#define __LOCKREF_ENABLE_CMPXCHG__
#endif
struct lockref
{
    union
    {
#ifdef __LOCKREF_ENABLE_CMPXCHG__
        aligned_u64 lock_count; // 通过该变量的声明，使得整个lockref按照8字节对齐
#endif
        struct
        {
            spinlock_t lock;
            int count;
        };
    };
};

/**
 * @brief 原子的将引用计数加1
 * 
 * @param lock_ref 要被操作的lockref变量
 */
void lockref_inc(struct lockref *lock_ref);

/**
 * @brief 原子地将引用计数加1.如果原来的count≤0，则操作失败。
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return bool  操作成功=>true
 *               操作失败=>false
 */
bool lockref_inc_not_zero(struct lockref *lock_ref);

/**
 * @brief 原子地减少引用计数。如果已处于count≤0的状态，则返回-1
 *
 * 本函数与lockref_dec_return()的区别在于，当在cmpxchg()中检测到count<=0或已加锁，本函数会再次尝试通过加锁来执行操作
 * 而后者会直接返回错误
 * 
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return int 操作成功 => 返回新的引用变量值
 *             lockref处于count≤0的状态 => 返回-1
 */
int lockref_dec(struct lockref *lock_ref);

/**
 * @brief 原子地减少引用计数。如果处于已加锁或count≤0的状态，则返回-1
 *
 * 本函数与lockref_dec()的区别在于，当在cmpxchg()中检测到count<=0或已加锁，本函数会直接返回错误
 * 而后者会再次尝试通过加锁来执行操作
 * 
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return int  操作成功 => 返回新的引用变量值
 *              lockref处于已加锁或count≤0的状态 => 返回-1
 */
int lockref_dec_return(struct lockref *lock_ref);


/**
 * @brief 原子地减少引用计数。若当前的引用计数≤1，则操作失败
 * 
 * 该函数与lockref_dec_or_lock_not_zero()的区别在于，当cmpxchg()时发现old.count≤1时，该函数会直接返回false.
 * 而后者在这种情况下，会尝试加锁来进行操作。
 * 
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return true 成功将引用计数减1
 * @return false 如果当前的引用计数≤1，操作失败
 */
bool lockref_dec_not_zero(struct lockref *lock_ref);

/**
 * @brief 原子地减少引用计数。若当前的引用计数≤1，则操作失败
 *
 * 该函数与lockref_dec_not_zero()的区别在于，当cmpxchg()时发现old.count≤1时，该函数会尝试加锁来进行操作。
 * 而后者在这种情况下，会直接返回false.
 *
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return true 成功将引用计数减1
 * @return false 如果当前的引用计数≤1，操作失败
 */
bool lockref_dec_or_lock_not_zero(struct lockref *lock_ref);

/**
 * @brief 将lockref变量标记为已经死亡（将count设置为负值）
 * 
 * @param lock_ref 指向要被操作的lockref变量的指针
 */
void lockref_mark_dead(struct lockref * lock_ref);

/**
 * @brief 自增引用计数。（除非该lockref已经死亡）
 * 
 * @param lock_ref 指向要被操作的lockref变量的指针
 * @return true 操作成功
 * @return false 操作失败，lockref已死亡
 */
bool lockref_inc_not_dead(struct lockref *lock_ref);