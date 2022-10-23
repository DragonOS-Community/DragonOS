#include <common/stddef.h>

/**
 * @brief 统计二进制数的前导0
 *
 * @param x 待统计的数
 * @return int 结果
 */
static __always_inline int __clz(uint32_t x)
{
    asm volatile("bsr %%eax, %%eax\n\t"
                 "xor $0x1f, %%eax\n\t"
                 : "=a"(x)
                 : "a"(x)
                 : "memory");
    return x;
}

/**
 * @brief 统计二进制数的前导0 (宽度为unsigned long)
 *
 * @param x 待统计的数
 * @return int 结果
 */
static __always_inline int __clzl(unsigned long x)
{
    int res = 0;
    asm volatile("cltq\n\t"
                 "bsr %%rax, %%rax\n\t"
                 "xor $0x3f, %%rax\n\t"
                 "mov %%eax,%0\n\t"
                 : "=m"(res)
                 : "a"(x)
                 : "memory");
    return res;
}

/**
 * @brief 统计二进制数的前导0（宽度为unsigned long long）
 *
 * @param x 待统计的数
 * @return int 结果
 */
static __always_inline int __clzll(unsigned long long x)
{
    int res = 0;
    asm volatile("cltq\n\t"
                 "bsr %%rax, %%rax\n\t"
                 "xor $0x3f, %%rax\n\t"
                 "mov %%eax,%0\n\t"
                 : "=m"(res)
                 : "a"(x)
                 : "memory");
    return res;
}

static __always_inline int __ctz(uint32_t x)
{
    asm volatile("tzcnt %%eax, %%eax":"=a"(x):"a"(x):"memory");
    return x;
}

static __always_inline int __ctzl(unsigned long x)
{
    asm volatile("tzcnt %%rax, %%rax":"=a"(x):"a"(x):"memory");
    return x;
}

#define __ctzll __ctzl