/**
 * @file gate.h
 * @author longjin
 * @brief 门定义
 * @date 2022-01-24
 * 
 */

#pragma once


/**
 * @brief 初始化中段描述符表内的门描述符（每个16B）
 * @param gate_selector_addr IDT表项的地址
 * @param attr P、DPL、TYPE的属性
 * @param ist 中断栈表号
 * @param code_addr 中断服务程序的地址
 */
// todo:在系统异常处理主功能完成后，将这段代码用C来写一遍。这段汇编实在是太晦涩难懂了，我看了半个钟才看明白。
#define _set_gate(gate_selector_addr, attr, ist, code_addr) \
do{
    unsigned long __d0, __d1;                               \
    __asm__ __volatile__ (  "movw   %%dx,   %%ax    \n\t"   \
                            "andq   $0x7,   %%rcx   \n\t"   \   // 清空rcx中除了2:0以外的所有位(此前ist的值已经被赋给了rcx)
                            "addq   %4,     %%rcx   \n\t"   \   // 将P,DPL, Type的值加到rcx中
                            "shlq   $32,    %%rcx   \n\t"   \ 
                            "addq   %%rcx,  %%rax   \n\t"   \   // 设置ist
                            "xorq   %%rcx,  %%rcx   \n\t"   \   // 清空rcx
                            "movl   %%edx,  %%ecx   \n\t"   \ 
                            "shrq   $16,    %%ecx   \n\t"   \
                            "shlq   $48,    %%rcx   \n\t"   \   // 左移到低8B中表示段内偏移的[31:16]处
                            "addq   %%rcx,  %%rax   \n\t"   \   // 设置段内偏移[31:16]
                            "movq   %%rax,  %0      \n\t"   \   // 输出到门选择子的低8B
                            "shrq   $32,    %%rdx   \n\t"   \
                            "movq   %%rdx,  %1      \n\t"   \   // 输出到门选择子的高8B
                            :"=m"(*((unsigned long *)(gate_selector_addr)))	,					                \
					            "=m"(*(1 + (unsigned long *)(gate_selector_addr))),"=&a"(__d0),"=&d"(__d1)		\
					        :"i"(attr << 8),									                                \
					            "3"((unsigned long *)(code_addr)),"2"(0x8 << 16),"c"(ist)				        \
					        :"memory"		                                                                    \
    )
}while(0)
