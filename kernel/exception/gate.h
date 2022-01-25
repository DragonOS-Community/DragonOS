/**
 * @file gate.h
 * @author longjin
 * @brief 门定义
 * @date 2022-01-24
 * 
 */

#pragma once

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

extern struct desc_struct GDT_Table[]; //GDT_Table是head.S中的GDT_Table
extern struct gate_struct IDT_Table[]; //IDT_Table是head.S中的IDT_Table
extern unsigned int TSS64_Table[26];

/**
 * @brief 初始化中段描述符表内的门描述符（每个16B）
 * @param gate_selector_addr IDT表项的地址
 * @param attr P、DPL、TYPE的属性
 * @param ist 中断栈表号
 * @param code_addr 中断服务程序的地址
 */
// todo:在系统异常处理主功能完成后，将这段代码用C来写一遍。这段汇编实在是太晦涩难懂了，我看了半个钟才看明白。

/*
#define _set_gate(gate_selector_addr, attr, ist, code_addr) \
do{                                                         \
    unsigned long __d0, __d1;                               \
    __asm__ __volatile__ (  "movw   %%dx,   %%ax    \n\t"   \
                            "andq   $0x7,   %%rcx   \n\t"   // 清空rcx中除了2:0以外的所有位 此前ist的值已经被赋给了rcx \   
                            "addq   %4,     %%rcx   \n\t"   // 将P,DPL, Type的值加到rcx中 \   
                            "shlq   $32,    %%rcx   \n\t"   \ 
                            "addq   %%rcx,  %%rax   \n\t"   // 设置ist \   
                            "xorq   %%rcx,  %%rcx   \n\t"   // 清空rcx \   
                            "movl   %%edx,  %%ecx   \n\t"   \ 
                            "shrq   $16,    %%ecx   \n\t"   \
                            "shlq   $48,    %%rcx   \n\t"   // 左移到低8B中表示段内偏移的[31:16]处 \   
                            "addq   %%rcx,  %%rax   \n\t"  // 设置段内偏移[31:16] \   
                            "movq   %%rax,  %0      \n\t"   // 输出到门选择子的低8B \   
                            "shrq   $32,    %%rdx   \n\t"   \
                            "movq   %%rdx,  %1      \n\t"   // 输出到门选择子的高8B \   
                            :"=m"(*((unsigned long *)(gate_selector_addr)))	,					                \
					            "=m"(*(1 + (unsigned long *)(gate_selector_addr))),"=&a"(__d0),"=&d"(__d1)		\
					        :"i"(attr << 8),									                                \
					            "3"((unsigned long *)(code_addr)),"2"(0x8 << 16),"c"(ist)				        \
					        :"memory"		                                                                    \
    );                                                                                                          \
}while(0)
*/
//由于带上注释就编译不过，因此复制一份到这里
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

/**
 * @brief 加载任务状态段寄存器
 * @param n TSS基地址在GDT中的第几项
 * 左移3位的原因是GDT每项占8字节
 */
#define load_TR(n)                                     \
    do                                                 \
    {                                                  \
        __asm__ __volatile__("ltr %%ax" ::"a"(n << 3)); \
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
    _set_gate(IDT_Table + n, 0x8E, ist, addr); // p=1，DPL=0, type=E
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
    _set_gate(IDT_Table + n, 0x8F, ist, addr); // p=1，DPL=0, type=F
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
    _set_gate(IDT_Table + n, 0xEF, ist, addr); // p=1，DPL=3, type=F
}

/**
 * @brief 初始化TSS表的内容
 * 
 */
void set_TSS64(ul rsp0, ul rsp1, ul rsp2, ul ist1, ul ist2, ul ist3, ul ist4, ul ist5, ul ist6, ul ist7)
{
    *(ul *)(TSS64_Table + 1) = rsp0;
    *(ul *)(TSS64_Table + 3) = rsp1;
    *(ul *)(TSS64_Table + 5) = rsp2;

    *(ul *)(TSS64_Table + 9) = ist1;
    *(ul *)(TSS64_Table + 11) = ist2;
    *(ul *)(TSS64_Table + 13) = ist3;
    *(ul *)(TSS64_Table + 15) = ist4;
    *(ul *)(TSS64_Table + 17) = ist5;
    *(ul *)(TSS64_Table + 19) = ist6;
    *(ul *)(TSS64_Table + 21) = ist7;
}