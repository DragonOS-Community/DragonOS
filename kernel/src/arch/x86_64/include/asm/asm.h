#pragma once

#include <DragonOS/stdint.h>
#include <stdbool.h>
#include <common/stddef.h>



#define sti() __asm__ __volatile__("sti\n\t" :: \
                                       : "memory") // 开启外部中断
#define cli() __asm__ __volatile__("cli\n\t" :: \
                                       : "memory") // 关闭外部中断
#define nop() __asm__ __volatile__("nop\n\t")
#define hlt() __asm__ __volatile__("hlt\n\t")
#define pause() asm volatile("pause\n\t"); // 处理器等待一段时间

// 内存屏障
#define io_mfence() __asm__ __volatile__("mfence\n\t" :: \
                                             : "memory") // 在mfence指令前的读写操作必须在mfence指令后的读写操作前完成。
#define io_sfence() __asm__ __volatile__("sfence\n\t" :: \
                                             : "memory") // 在sfence指令前的写操作必须在sfence指令后的写操作前完成
#define io_lfence() __asm__ __volatile__("lfence\n\t" :: \
                                             : "memory") // 在lfence指令前的读操作必须在lfence指令后的读操作前完成。

/*
 * Macros to generate condition code outputs from inline assembly,
 * The output operand must be type "bool".
 */
// 如果编译器支持输出标志寄存器值到变量的话，则会定义__GCC_ASM_FLAG_OUTPUTS__
#ifdef __GCC_ASM_FLAG_OUTPUTS__
// CC_SET(c)则是用于设置标志寄存器中的某一位
#define CC_SET(c) "\n\t/* output condition code " #c "*/\n"
// "=@cccond"的用法是，将标志寄存器中的cond（也就是指令集定义的标准条件）的值输出到变量中
#define CC_OUT(c) "=@cc" #c
#else
#define CC_SET(c) "\n\tset" #c " %[_cc_" #c "]\n"
#define CC_OUT(c) [_cc_##c] "=qm"
#endif

#define rdtsc() ({                                    \
    uint64_t tmp1 = 0, tmp2 = 0;                      \
    asm volatile("rdtsc"                              \
                 : "=d"(tmp1), "=a"(tmp2)::"memory"); \
    (tmp1 << 32 | tmp2);                              \
})

/**
 * @brief 读取rsp寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rsp的值的指针
 */
unsigned long *get_rsp()
{
    uint64_t *tmp;
    __asm__ __volatile__(
        "movq %%rsp, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

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
 * @brief 读取rbp寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rbp的值的指针
 */
unsigned long *get_rbp()
{
    uint64_t *tmp;
    __asm__ __volatile__(
        "movq %%rbp, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

/**
 * @brief 读取ds寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  ds的值的指针
 */
unsigned long *get_ds()
{
    uint64_t *tmp;
    __asm__ __volatile__(
        "movq %%ds, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

/**
 * @brief 读取rax寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rax的值的指针
 */
unsigned long *get_rax()
{
    uint64_t *tmp;
    __asm__ __volatile__(
        "movq %%rax, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}
/**
 * @brief 读取rbx寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rbx的值的指针
 */
unsigned long *get_rbx()
{
    uint64_t *tmp;
    __asm__ __volatile__(
        "movq %%rbx, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

uint64_t get_rflags()
{
    unsigned long tmp = 0;
    __asm__ __volatile__("pushfq	\n\t"
                         "movq	(%%rsp), %0	\n\t"
                         "popfq	\n\t"
                         : "=r"(tmp)::"memory");
    return tmp;
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
    if (verify_area((uint64_t)src, size))
        return 0;

    /**
     * @brief 先每次搬运8 bytes，剩余就直接一个个byte搬运
     *
     */
    // todo:编译有bug
    // asm volatile("rep   \n\t"
    //              "movsq  \n\t"
    //              "movq %3, %0   \n\t"
    //              "rep   \n\t"
    //              "movsb \n\t"
    //              : "=&c"(size), "=&D"(tmp0), "=&S"(tmp1)
    //              : "r"(size & 7), "0"(size >> 3), "1"(dst), "2"(src)
    //              : "memory");
    memcpy(dst, src, size);

    return size;
}

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
 * @brief 逐字节比较指定内存区域的值，并返回s1、s2的第一个不相等的字节i处的差值（s1[i]-s2[i])。
 * 若两块内存区域的内容相同，则返回0
 *
 * @param s1 内存区域1
 * @param s2 内存区域2
 * @param len 要比较的内存区域长度
 * @return int s1、s2的第一个不相等的字节i处的差值（s1[i]-s2[i])。若两块内存区域的内容相同，则返回0
 */
static inline int memcmp(const void *s1, const void *s2, size_t len)
{
    int diff;

    asm("cld \n\t"  // 复位DF，确保s1、s2指针是自增的
        "repz; cmpsb\n\t" CC_SET(nz)
        : CC_OUT(nz)(diff), "+D"(s1), "+S"(s2)
        : "c"(len)
        : "memory");

    if (diff)
        diff = *(const unsigned char *)(s1 - 1) - *(const unsigned char *)(s2 - 1);

    return diff;
}
