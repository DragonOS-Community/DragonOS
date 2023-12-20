/**
 * @file atomic.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief 原子变量
 * @version 0.1
 * @date 2022-04-12
 *
 * @copyright Copyright (c) 2022
 *
 */
#pragma once
#if ARCH(I386) || ARCH(X86_64)

#include <arch/x86_64/include/asm/cmpxchg.h>

#define atomic_read(atomic) ((atomic)->value)               // 读取原子变量
#define atomic_set(atomic, val) (((atomic)->value) = (val)) // 设置原子变量的初始值

typedef struct
{
    volatile long value;
} atomic_t;

/**
 * @brief 原子变量增加值
 *
 * @param ato 原子变量对象
 * @param val 要增加的值
 */
inline void atomic_add(atomic_t *ato, long val)
{
    asm volatile("lock addq %1, %0 \n\t"
                 : "=m"(ato->value)
                 : "m"(val)
                 : "memory");
}

/**
 * @brief 原子变量减少值
 *
 * @param ato 原子变量对象
 * @param val 要减少的值
 */
inline void atomic_sub(atomic_t *ato, long val)
{
    asm volatile("lock subq %1, %0  \n\t"
                 : "=m"(ato->value)
                 : "m"(val)
                 : "memory");
}

/**
 * @brief 原子变量自增
 *
 * @param ato 原子变量对象
 */
void atomic_inc(atomic_t *ato)
{
    asm volatile("lock incq %0   \n\t"
                 : "=m"(ato->value)
                 : "m"(ato->value)
                 : "memory");
}

/**
 * @brief 原子变量自减
 *
 * @param ato 原子变量对象
 */
void atomic_dec(atomic_t *ato)
{
    asm volatile("lock decq %0 \n\t"
                 : "=m"(ato->value)
                 : "m"(ato->value)
                 : "memory");
}

/**
 * @brief 设置原子变量的mask
 *
 * @param ato 原子变量对象
 */
inline void atomic_set_mask(atomic_t *ato, long mask)
{
    __asm__ __volatile__("lock	orq	%1,	%0	\n\t"
                         : "=m"(ato->value)
                         : "r"(mask)
                         : "memory");
}

/**
 * @brief 清除原子变量的mask
 *
 * @param ato 原子变量对象
 */
inline void atomic_clear_mask(atomic_t *ato, long mask)
{
    __asm__ __volatile__("lock	andq	%1,	%0	\n\t"
                         : "=m"(ato->value)
                         : "r"(mask)
                         : "memory");
}

// cmpxchgq 比较并交换
inline long atomic_cmpxchg(atomic_t *ato, long oldval, long newval)
{
    bool success = arch_try_cmpxchg(&ato->value, &oldval, newval);
    return success ? oldval : newval;
}
#endif