#pragma once
#include <common/compiler.h>
#include <asm/asm.h>

/**
 * @brief 通过extern不存在的函数，来让编译器报错。以防止不符合要求的代码的产生。
 */
extern void __cmpxchg_wrong_size(void) __compiletime_error("Bad argument size for cmpxchg");

// 定义常量：操作符涉及到的字节数
#define __X86_CASE_B 1
#define __X86_CASE_W 2
#define __X86_CASE_L 4
#define __X86_CASE_Q 8

/**
 * @brief lock cmpxchg指令的包装。
 * 将_ptr指向的值与old_ptr指向的值做比较，如果相等，则将_new指向的值，加载到_ptr指向的值中。
 */
#define __raw_try_cmpxchg(_ptr, _old_ptr, _new, size)               \
    ({                                                              \
        bool is_success = false;                                    \
        typeof(_ptr) _old = (typeof(_ptr))(_old_ptr);               \
        typeof(*(_ptr)) __old = *_old;                              \
        typeof(*(_ptr)) __new = (_new);                             \
        switch (size)                                               \
        {                                                           \
        case __X86_CASE_B:                                          \
        {                                                           \
            volatile uint8_t *__ptr = (volatile uint8_t *)(_ptr);   \
            asm volatile("lock cmpxchgb %[new], %[ptr]\n\t"         \
                         : CC_OUT(z)(is_success),                   \
                           [ptr] "+m"(*__ptr),                      \
                           [old] "+a"(__old)                        \
                         : [new] "q"(__new)                         \
                         : "memory");                               \
            break;                                                  \
        }                                                           \
        case __X86_CASE_W:                                          \
        {                                                           \
            volatile uint16_t *__ptr = (volatile uint16_t *)(_ptr); \
            asm volatile("lock cmpxchgw %[new], %[ptr]\n\t"         \
                         : CC_OUT(z)(is_success),                   \
                           [ptr] "+m"(*__ptr),                      \
                           [old] "+a"(__old)                        \
                         : [new] "q"(__new)                         \
                         : "memory");                               \
            break;                                                  \
        }                                                           \
        case __X86_CASE_L:                                          \
        {                                                           \
            volatile uint32_t *__ptr = (volatile uint32_t *)(_ptr); \
            asm volatile("lock cmpxchgl %[new], %[ptr]\n\t"         \
                         : CC_OUT(z)(is_success),                   \
                           [ptr] "+m"(*__ptr),                      \
                           [old] "+a"(__old)                        \
                         : [new] "q"(__new)                         \
                         : "memory");                               \
            break;                                                  \
        }                                                           \
        case __X86_CASE_Q:                                          \
        {                                                           \
            volatile uint64_t *__ptr = (volatile uint64_t *)(_ptr); \
            asm volatile("lock cmpxchgq %[new], %[ptr]\n\t"         \
                         : CC_OUT(z)(is_success),                   \
                           [ptr] "+m"(*__ptr),                      \
                           [old] "+a"(__old)                        \
                         : [new] "q"(__new)                         \
                         : "memory");                               \
            break;                                                  \
        }                                                           \
        default:                                                    \
            __cmpxchg_wrong_size();                                 \
        }                                                           \
        if (unlikely(is_success == false))                          \
            *_old = __old;                                          \
        likely(is_success);                                         \
    })

#define arch_try_cmpxchg(ptr, old_ptr, new) \
    __raw_try_cmpxchg((ptr), (old_ptr), (new), sizeof(*ptr))

bool __try_cmpxchg_q(uint64_t *ptr, uint64_t *old_ptr, uint64_t *new_ptr);
