// 这是内核执行头程序
// Created by longjin.
// 2022/01/20

#include "common/asm.h"

.section .text

.global _start

_start:
    // 初始化寄存器
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %fs
    mov %ax, %ss

    mov $0x7e00, %esp

// === 加载GDTR ====
    lgdt GDT_POINTER(%rip) //这里我没搞明白rip相对寻址, 看了文档，大概是用来实现PIC的（position independent code）
// === 加载IDTR ====
    lidt IDT_POINTER(%rip)
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %fs
    mov %ax, %ss
    mov %ax, %gs

    movq $0x7e00, %rsp

// ==== 加载CR3寄存器
    movq $0x101000, %rax //设置页目录基地址
    movq %rax, %cr3
    movq switch_seg(%rip), %rax

    // 由于ljmp和lcall在GAS中不受支持，因此我们需要先伪造函数调用现场，通过lret的方式，给它跳转过去。才能更新cs寄存器
    // 实在是太妙了！Amazing！
    pushq $0x08 //段选择子
    pushq %rax
    lretq

// 64位模式的代码
switch_seg:
    .quad entry64

entry64:
    movq $0x10, %rax
    movq %rax, %ds
    movq %rax, %es
    movq %rax, %gs
    movq %rax, %ss

    movq _stack_start(%rip), %rsp //rsp的地址

setup_IDT:
    leaq m_ignore_int(%rip),  %rdx // 将ignore_int的地址暂时存到中段描述符的高8B
    movq $(0x08 << 16), %rax  // 设置段选择子。由IDT结构和段选择子结构可知，本行设置段基地址为0x100000，TI=0,RPL=0
    movw %dx, %ax

    movq $ (0x8e00 << 32), %rcx // 设置Type=1110 P=1 DPL=00 0=0
    addq %rcx, %rax

    // 把ignore_int的地址填写到正确位置, rax存低8B， rdx存高8B
    movl %edx, %ecx
    shrl $16, %ecx // 去除低16位
    shlq $48, %rcx
    addq %rcx, %rax // 填写段内偏移31:16

    shrq $32, %rdx // （已经填写了32位，故右移32）

    leaq IDT_Table(%rip), %rdi // 获取中断描述符表的首地址，存储到rdi
    mov $256, %rcx  // 初始化每个中断描述符

repeat_set_idt:
    // ====== 循环，初始化总共256个中断描述符 ===
    movq %rax, (%rdi)   // 保存低8B
    movq %rdx, 8(%rdi)  // 保存高8B

    addq $0x10, %rdi // 转到下一个IDT表项
    dec %rcx
    jne repeat_set_idt

SetUp_TSS64:
    // == 设置64位的任务状态段表 ===
    //rdx保存高8B， rax保存低8B
    leaq TSS64_Table(%rip), %rdx
    xorq %rax, %rax
    xorq %rcx, %rcx

    // 设置TSS描述符的47：40位为1000 1001
    movq $0x89, %rax
    shlq $40, %rax

    // 设置段基地址31:24
    movl %edx, %ecx
    shrl $24, %ecx
    shlq $56, %rcx
    addq %rcx, %rax

    xorq %rcx, %rcx

    // 设置段基地址23:00
    movl %edx, %ecx
    andl $0xffffff, %ecx // 清空ecx的中有效值的高8位（也就是上面已经赋值了的）
    shlq $16, %rcx
    addq %rcx, %rax

    addq $103, %rax // 设置段长度

    leaq GDT_Table(%rip), %rdi
    movq %rax, 80(%rdi) // 把低八B存储到GDT第10项
    shrq $32, %rdx
    movq %rdx, 88(%rdi) // 高8B存到GDT第11项

    // 装载任务状态段寄存器(已改为在main.c中使用load_TR宏进行装载)
    // mov $0x50, %ax // 设置起始地址为80
    // ltr %ax

    // 切换到内核主程序
    movq go_to_kernel(%rip), %rax
    pushq $0x08
    pushq %rax
    lretq

go_to_kernel:
    .quad Start_Kernel

// ==== 异常/中断处理模块 ignore int： 忽略中断
m_ignore_int:
// 切换到c语言的ignore_int
    movq go_to_ignore_int(%rip), %rax
    pushq $0x08
    pushq %rax
    lretq

    lretq


go_to_ignore_int:
    .quad ignore_int


ENTRY(_stack_start)
    .quad initial_proc_union + 32768


// 初始化页表
.align 8 //设置为8byte对齐
.org 0x1000 //设置页表位置为内核执行头程序的0x1000处

__PML4E:
    .quad 0x102007 // 用户访问，可读写，已存在， 地址在31~12位
    .fill	255,8,0
	.quad	0x102007
	.fill	255,8,0

.org	0x2000

__PDPTE:

	.quad	0x103007 // 用户访问，可读写，已存在
	.fill	511,8,0

.org	0x3000

__PDE:

	.quad	0x000087    // 用户访问，可读写，已存在
	.quad	0x200087
	.quad	0x400087
	.quad	0x600087
	.quad	0x800087
	.quad	0xe0000087		/*0x a00000*/
	.quad	0xe0200087
	.quad	0xe0400087
	.quad	0xe0600087		/*0x1000000*/
	.quad	0xe0800087
	.quad	0xe0a00087
	.quad	0xe0c00087
	.quad	0xe0e00087
	.fill	499,8,0

// GDT表
.section .data
.global GDT_Table // 使得GDT可以被外部程序引用或者访问

GDT_Table:
    .quad 0x0000000000000000 // 0 空描述符 0x00
    .quad 0x0020980000000000 // 1 内核64位代码段描述符 0x08
    .quad 0x0000920000000000 // 2 内核64位数据段描述符 0x10
    .quad 0x0000000000000000 // 3 用户32位代码段描述符 0x18
    .quad 0x0000000000000000 // 4 用户32位数据段描述符 0x20
    .quad 0x0020f80000000000 // 5 用户64位代码段描述符 0x28
    .quad 0x0000f20000000000 // 6 用户64位数据段描述符 0x30
    .quad 0x00cf9a000000ffff // 7 内核32位代码段描述符 0x38
    .quad 0x00cf92000000ffff // 8 内核32位数据段描述符 0x40
    .fill 10, 8, 0           // 10-11 TSS(跳过了第9段)  重复十次填充8字节的空间，赋值为0
GDT_END:

GDT_POINTER:
GDT_LIMIT: .word GDT_END - GDT_Table - 1 // GDT的大小
GDT_BASE: .quad GDT_Table

// IDT 表
.global IDT_Table

IDT_Table:
    .fill 512, 8, 0 // 设置512*8字节的IDT表的空间
IDT_END:

IDT_POINTER:
IDT_LIMIT: .word IDT_END - IDT_Table - 1
IDT_BASE: .quad IDT_Table

// 64位的TSS表
.global TSS64_Table

TSS64_Table:
    .fill 13, 8, 0
TSS64_END:

TSS64_POINTER:
TSS64_LIMIT: .word TSS64_END - TSS64_Table - 1
TSS64_BASE: .quad TSS64_Table
