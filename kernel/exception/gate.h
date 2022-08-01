/**
 * @file gate.h
 * @author longjin
 * @brief 门定义
 * @date 2022-01-24
 *
 */

#ifndef __GATE_H__
#define __GATE_H__

#include <common/kprint.h>
#include <mm/mm.h>

#pragma GCC push_options
#pragma GCC optimize("O0")
//描述符表的结构体
struct desc_struct
{
    unsigned char x[8];
};

//门的结构体
struct gate_struct
{
    unsigned char x[16];
};

extern struct desc_struct GDT_Table[]; // GDT_Table是head.S中的GDT_Table
extern struct gate_struct IDT_Table[]; // IDT_Table是head.S中的IDT_Table
//extern unsigned int TSS64_Table[26];

struct gdtr
{
    uint16_t size;
    uint64_t gdt_vaddr;
}__attribute__((packed));

struct idtr
{
    uint16_t size;
    uint64_t idt_vaddr;
}__attribute__((packed));

/**
 * @brief 初始化中段描述符表内的门描述符（每个16B）
 * @param gate_selector_addr IDT表项的地址
 * @param attr P、DPL、TYPE的属性
 * @param ist 中断栈表号
 * @param code_addr 指向中断服务程序的指针的地址
 */

void set_gate(ul *gate_selector_addr, ul attr, unsigned char ist, ul *code_addr)
{
    ul __d0 = 0, __d1 = 0;
    ul tmp_code_addr = *code_addr;
    __d0 = attr << 40; //设置P、DPL、Type

    __d0 |= ((ul)(ist) << 32); // 设置ist

    __d0 |= 8 << 16; //设置段选择子为0x1000

    __d0 |= (0xffff & tmp_code_addr); //设置段内偏移的[15:00]

    tmp_code_addr >>= 16;
    __d0 |= (0xffff & tmp_code_addr) << 48; // 设置段内偏移[31:16]

    tmp_code_addr >>= 16;

    __d1 = (0xffffffff & tmp_code_addr); //设置段内偏移[63:32]

    *gate_selector_addr = __d0;
    *(gate_selector_addr + 1) = __d1;
}

#define _set_gate(gate_selector_addr, attr, ist, code_addr)                                                 \
    do                                                                                                      \
    {                                                                                                       \
        unsigned long __d0, __d1;                                                                           \
        __asm__ __volatile__("movw	%%dx,	%%ax	\n\t"                                                         \
                             "andq	$0x7,	%%rcx	\n\t"                                                        \
                             "addq	%4,	%%rcx	\n\t"                                                          \
                             "shlq	$32,	%%rcx	\n\t"                                                         \
                             "addq	%%rcx,	%%rax	\n\t"                                                       \
                             "xorq	%%rcx,	%%rcx	\n\t"                                                       \
                             "movl	%%edx,	%%ecx	\n\t"                                                       \
                             "shrq	$16,	%%rcx	\n\t"                                                         \
                             "shlq	$48,	%%rcx	\n\t"                                                         \
                             "addq	%%rcx,	%%rax	\n\t"                                                       \
                             "movq	%%rax,	%0	\n\t"                                                          \
                             "shrq	$32,	%%rdx	\n\t"                                                         \
                             "movq	%%rdx,	%1	\n\t"                                                          \
                             : "=m"(*((unsigned long *)(gate_selector_addr))),                              \
                               "=m"(*(1 + (unsigned long *)(gate_selector_addr))), "=&a"(__d0), "=&d"(__d1) \
                             : "i"(attr << 8),                                                              \
                               "3"((unsigned long *)(code_addr)), "2"(0x8 << 16), "c"(ist)                  \
                             : "memory");                                                                   \
    } while (0)

void set_tss_descriptor(unsigned int n, void *addr)
{

    unsigned long limit = 103;
    
    *(unsigned long *)(phys_2_virt(GDT_Table + n)) = (limit & 0xffff) | (((unsigned long)addr & 0xffff) << 16) | ((((unsigned long)addr >> 16) & 0xff) << 32) | ((unsigned long)0x89 << 40) | ((limit >> 16 & 0xf) << 48) | (((unsigned long)addr >> 24 & 0xff) << 56); /////89 is attribute
    *(unsigned long *)(phys_2_virt(GDT_Table + n + 1)) = (((unsigned long)addr >> 32) & 0xffffffff) | 0;
}

/**
 * @brief 加载任务状态段寄存器
 * @param n TSS基地址在GDT中的第几项
 * 左移3位的原因是GDT每项占8字节
 */
#define load_TR(n)                                        \
    do                                                    \
    {                                                     \
        __asm__ __volatile__("ltr %%ax" ::"a"((n) << 3)); \
    } while (0)

/**
 * @brief 设置中断门
 *
 * @param n 中断号
 * @param ist ist
 * @param addr 服务程序的地址
 */
void set_intr_gate(unsigned int n, unsigned char ist, void *addr)
{
    _set_gate(phys_2_virt(IDT_Table + n), 0x8E, ist, addr); // p=1，DPL=0, type=E
    
    //set_gate((ul *)phys_2_virt(IDT_Table + n), 0x8E, ist, (ul *)(addr)); // p=1，DPL=0, type=E
}

/**
 * @brief 设置64位，DPL=0的陷阱门
 *
 * @param n 中断号
 * @param ist ist
 * @param addr 服务程序的地址
 */
void set_trap_gate(unsigned int n, unsigned char ist, void *addr)
{
    // kdebug("addr=%#018lx", (ul)(addr));

    //set_gate((ul *)phys_2_virt(IDT_Table + n), 0x8F, ist, (ul *)(addr)); // p=1，DPL=0, type=F
    _set_gate(phys_2_virt(IDT_Table + n), 0x8F, ist, addr); // p=1，DPL=0, type=F
}

/**
 * @brief 设置64位，DPL=3的陷阱门
 *
 * @param n 中断号
 * @param ist ist
 * @param addr 服务程序的地址
 */
void set_system_trap_gate(unsigned int n, unsigned char ist, void *addr)
{
    // kdebug("addr=%#018lx", (ul)(addr));

    //set_gate((ul *)phys_2_virt(IDT_Table + n), 0xEF, ist, (ul *)(addr)); // p=1，DPL=3, type=F
    _set_gate(phys_2_virt(IDT_Table + n), 0xEF, ist, addr); // p=1，DPL=3, type=F
}


static inline void set_system_intr_gate(unsigned int n,unsigned char ist,void * addr)	//int3
{
	_set_gate(phys_2_virt(IDT_Table + n) , 0xEE , ist , addr);	//P,DPL=3,TYPE=E
}
/**
 * @brief 初始化TSS表的内容
 *
 */

void set_tss64(unsigned int *Table, unsigned long rsp0, unsigned long rsp1, unsigned long rsp2, unsigned long ist1, unsigned long ist2, unsigned long ist3,
               unsigned long ist4, unsigned long ist5, unsigned long ist6, unsigned long ist7)
{
    *(unsigned long *)(Table + 1) = rsp0;
    *(unsigned long *)(Table + 3) = rsp1;
    *(unsigned long *)(Table + 5) = rsp2;

    *(unsigned long *)(Table + 9) = ist1;
    *(unsigned long *)(Table + 11) = ist2;
    *(unsigned long *)(Table + 13) = ist3;
    *(unsigned long *)(Table + 15) = ist4;
    *(unsigned long *)(Table + 17) = ist5;
    *(unsigned long *)(Table + 19) = ist6;
    *(unsigned long *)(Table + 21) = ist7;
}
#endif

#pragma GCC pop_options