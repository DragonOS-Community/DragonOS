#pragma once

#include <stdint.h>

#define sti() __asm__ __volatile__("sti\n\t" :: \
                                       : "memory") //开启外部中断
#define cli() __asm__ __volatile__("cli\n\t" :: \
                                       : "memory") //关闭外部中断
#define nop() __asm__ __volatile__("nop\n\t")
#define hlt() __asm__ __volatile__("hlt\n\t")
#define pause() asm volatile("pause\n\t"); // 处理器等待一段时间

//内存屏障
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

// ========= MSR寄存器组操作 =============
/**
 * @brief 向msr寄存器组的address处的寄存器写入值value
 *
 * @param address 地址
 * @param value 要写入的值
 */
void wrmsr(uint64_t address, uint64_t value)
{
    __asm__ __volatile__("wrmsr    \n\t" ::"d"(value >> 32), "a"(value & 0xffffffff), "c"(address)
                         : "memory");
}

/**
 * @brief 从msr寄存器组的address地址处读取值
 * rdmsr返回高32bits在edx，低32bits在eax
 * @param address 地址
 * @return uint64_t address处的寄存器的值
 */
uint64_t rdmsr(uint64_t address)
{
    unsigned int tmp0, tmp1;
    __asm__ __volatile__("rdmsr \n\t"
                         : "=d"(tmp0), "=a"(tmp1)
                         : "c"(address)
                         : "memory");
    return ((uint64_t)tmp0 << 32) | tmp1;
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