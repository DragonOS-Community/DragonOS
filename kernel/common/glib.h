//
// 内核全局通用库
// Created by longjin on 2022/1/22.
//

#pragma once

//引入对bool类型的支持
#include <stdbool.h>
#include <stdint.h>
#include <common/stddef.h>
#include <arch/arch.h>
#include <common/compiler.h>
#include <common/list.h>

#if ARCH(I386) || ARCH(X86_64)
#include <arch/x86_64/asm/asm.h>
#else
#error Arch not supported.
#endif

/**
 * @brief 根据结构体变量内某个成员变量member的基地址，计算出该结构体变量的基地址
 * @param ptr 指向结构体变量内的成员变量member的指针
 * @param type 成员变量所在的结构体
 * @param member 成员变量名
 *
 * 方法：使用ptr减去结构体内的偏移，得到结构体变量的基地址
 */
#define container_of(ptr, type, member)                                     \
    ({                                                                      \
        typeof(((type *)0)->member) *p = (ptr);                             \
        (type *)((unsigned long)p - (unsigned long)&(((type *)0)->member)); \
    })

// 定义类型的缩写
typedef unsigned char uchar;
typedef unsigned short ushort;
typedef unsigned int uint;
typedef unsigned long ul;
typedef unsigned long long int ull;
typedef long long int ll;

#define ABS(x) ((x) > 0 ? (x) : -(x)) // 绝对值
// 最大最小值
#define max(x, y) ((x > y) ? (x) : (y))
#define min(x, y) ((x < y) ? (x) : (y))

// 遮罩高32bit
#define MASK_HIGH_32bit(x) (x & (0x00000000ffffffffUL))

// 四舍五入成整数
ul round(double x)
{
    return (ul)(x + 0.5);
}

/**
 * @brief 地址按照align进行对齐
 *
 * @param addr
 * @param _align
 * @return ul 对齐后的地址
 */
static __always_inline ul ALIGN(const ul addr, const ul _align)
{
    return (ul)((addr + _align - 1) & (~(_align - 1)));
}


void *memset(void *dst, unsigned char C, ul size)
{

    int d0, d1;
    unsigned long tmp = C * 0x0101010101010101UL;
    __asm__ __volatile__("cld	\n\t"
                         "rep	\n\t"
                         "stosq	\n\t"
                         "testb	$4, %b3	\n\t"
                         "je	1f	\n\t"
                         "stosl	\n\t"
                         "1:\ttestb	$2, %b3	\n\t"
                         "je	2f\n\t"
                         "stosw	\n\t"
                         "2:\ttestb	$1, %b3	\n\t"
                         "je	3f	\n\t"
                         "stosb	\n\t"
                         "3:	\n\t"
                         : "=&c"(d0), "=&D"(d1)
                         : "a"(tmp), "q"(size), "0"(size / 8), "1"(dst)
                         : "memory");
    return dst;
}

void *memset_c(void *dst, uint8_t c, size_t count)
{
    uint8_t *xs = (uint8_t *)dst;

    while (count--)
        *xs++ = c;

    return dst;
}

/**
 * @brief 内存拷贝函数
 *
 * @param dst 目标数组
 * @param src 源数组
 * @param Num 字节数
 * @return void*
 */
static void *memcpy(void *dst, const void *src, long Num)
{
    int d0 = 0, d1 = 0, d2 = 0;
    __asm__ __volatile__("cld	\n\t"
                         "rep	\n\t"
                         "movsq	\n\t"
                         "testb	$4,%b4	\n\t"
                         "je	1f	\n\t"
                         "movsl	\n\t"
                         "1:\ttestb	$2,%b4	\n\t"
                         "je	2f	\n\t"
                         "movsw	\n\t"
                         "2:\ttestb	$1,%b4	\n\t"
                         "je	3f	\n\t"
                         "movsb	\n\t"
                         "3:	\n\t"
                         : "=&c"(d0), "=&D"(d1), "=&S"(d2)
                         : "0"(Num / 8), "q"(Num), "1"(dst), "2"(src)
                         : "memory");
    return dst;
}

// 从io口读入8个bit
unsigned char io_in8(unsigned short port)
{
    unsigned char ret = 0;
    __asm__ __volatile__("inb	%%dx,	%0	\n\t"
                         "mfence			\n\t"
                         : "=a"(ret)
                         : "d"(port)
                         : "memory");
    return ret;
}

// 从io口读入32个bit
unsigned int io_in32(unsigned short port)
{
    unsigned int ret = 0;
    __asm__ __volatile__("inl	%%dx,	%0	\n\t"
                         "mfence			\n\t"
                         : "=a"(ret)
                         : "d"(port)
                         : "memory");
    return ret;
}

// 输出8个bit到输出端口
void io_out8(unsigned short port, unsigned char value)
{
    __asm__ __volatile__("outb	%0,	%%dx	\n\t"
                         "mfence			\n\t"
                         :
                         : "a"(value), "d"(port)
                         : "memory");
}

// 输出32个bit到输出端口
void io_out32(unsigned short port, unsigned int value)
{
    __asm__ __volatile__("outl	%0,	%%dx	\n\t"
                         "mfence			\n\t"
                         :
                         : "a"(value), "d"(port)
                         : "memory");
}

/**
 * @brief 从端口读入n个word到buffer
 *
 */
#define io_insw(port, buffer, nr)                                                 \
    __asm__ __volatile__("cld;rep;insw;mfence;" ::"d"(port), "D"(buffer), "c"(nr) \
                         : "memory")

/**
 * @brief 从输出buffer中的n个word到端口
 *
 */
#define io_outsw(port, buffer, nr)                                                 \
    __asm__ __volatile__("cld;rep;outsw;mfence;" ::"d"(port), "S"(buffer), "c"(nr) \
                         : "memory")


/**
 * @brief 验证地址空间是否为用户地址空间
 *
 * @param addr_start 地址起始值
 * @param length 地址长度
 * @return true
 * @return false
 */
bool verify_area(uint64_t addr_start, uint64_t length)
{
    if ((addr_start + length) <= 0x00007fffffffffffUL) // 用户程序可用的的地址空间应<= 0x00007fffffffffffUL
        return true;
    else
        return false;
}

/**
 * @brief 从用户空间搬运数据到内核空间
 *
 * @param dst 目的地址
 * @param src 源地址
 * @param size 搬运的大小
 * @return uint64_t
 */
static inline uint64_t copy_from_user(void *dst, void *src, uint64_t size)
{
    uint64_t tmp0, tmp1;
    if (!verify_area((uint64_t)src, size))
        return 0;

    /**
     * @brief 先每次搬运8 bytes，剩余就直接一个个byte搬运
     *
     */
    asm volatile("rep   \n\t"
                 "movsq  \n\t"
                 "movq %3, %0   \n\t"
                 "rep   \n\t"
                 "movsb \n\t"
                 : "=&c"(size), "=&D"(tmp0), "=&S"(tmp1)
                 : "r"(size & 7), "0"(size >> 3), "1"(dst), "2"(src)
                 : "memory");
    return size;
}

/**
 * @brief 从内核空间搬运数据到用户空间
 *
 * @param dst 目的地址
 * @param src 源地址
 * @param size 搬运的大小
 * @return uint64_t
 */
static inline uint64_t copy_to_user(void *dst, void *src, uint64_t size)
{
    uint64_t tmp0, tmp1;
    if (verify_area((uint64_t)src, size))
        return 0;

    /**
     * @brief 先每次搬运8 bytes，剩余就直接一个个byte搬运
     *
     */
    asm volatile("rep   \n\t"
                 "movsq  \n\t"
                 "movq %3, %0   \n\t"
                 "rep   \n\t"
                 "movsb \n\t"
                 : "=&c"(size), "=&D"(tmp0), "=&S"(tmp1)
                 : "r"(size & 7), "0"(size >> 3), "1"(dst), "2"(src)
                 : "memory");
    return size;
}

/**
 * @brief 这个函数让蜂鸣器发声，目前仅用于真机调试。未来将移除，请勿依赖此函数。
 *
 * @param times 发声循环多少遍
 */
void __experimental_beep(uint64_t times);

/**
 * @brief 往指定地址写入8字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @param value 要写入的值
 */
static __always_inline void __write8b(uint64_t vaddr, uint64_t value)
{
    asm volatile("movq %%rdx, 0(%%rax)" ::"a"(vaddr), "d"(value)
                 : "memory");

}

/**
 * @brief 往指定地址写入4字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @param value 要写入的值
 */
static __always_inline void __write4b(uint64_t vaddr, uint32_t value)
{
    asm volatile("movl %%edx, 0(%%rax)" ::"a"(vaddr), "d"(value)
                 : "memory");

}

/**
 * @brief 从指定地址读取8字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @return uint64_t 读取到的值
 */
static __always_inline uint64_t __read8b(uint64_t vaddr)
{
    uint64_t retval;
    asm volatile("movq 0(%%rax), %0"
                 : "=r"(retval)
                 : "a"(vaddr)
                 : "memory");
    return retval;
}

/**
 * @brief 从指定地址读取4字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @return uint64_t 读取到的值
 */
static __always_inline uint32_t __read4b(uint64_t vaddr)
{
    uint32_t retval;
    asm volatile("movl 0(%%rax), %0"
                 : "=d"(retval)
                 : "a"(vaddr)
                 : "memory");
    return retval;
}

/**
 * @brief 将数据从src搬运到dst，并能正确处理地址重叠的问题
 * 
 * @param dst 目标地址指针
 * @param src 源地址指针
 * @param size 大小
 * @return void* 指向目标地址的指针
 */
void *memmove(void *dst, const void *src, uint64_t size);