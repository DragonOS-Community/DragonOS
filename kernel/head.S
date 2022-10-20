// 这是内核执行头程序
// Created by longjin.
// 2022/01/20

#include "common/asm.h"

// 以下是来自 multiboot2 规范的定义
//  How many bytes from the start of the file we search for the header.
#define MULTIBOOT_SEARCH 32768
#define MULTIBOOT_HEADER_ALIGN 8

//  The magic field should contain this.
#define MULTIBOOT2_HEADER_MAGIC 0xe85250d6

//  This should be in %eax.
#define MULTIBOOT2_BOOTLOADER_MAGIC 0x36d76289

//  Alignment of multiboot modules.
#define MULTIBOOT_MOD_ALIGN 0x00001000

//  Alignment of the multiboot info structure.
#define MULTIBOOT_INFO_ALIGN 0x00000008

//  Flags set in the 'flags' member of the multiboot header.

#define MULTIBOOT_TAG_ALIGN 8
#define MULTIBOOT_TAG_TYPE_END 0
#define MULTIBOOT_TAG_TYPE_CMDLINE 1
#define MULTIBOOT_TAG_TYPE_BOOT_LOADER_NAME 2
#define MULTIBOOT_TAG_TYPE_MODULE 3
#define MULTIBOOT_TAG_TYPE_BASIC_MEMINFO 4
#define MULTIBOOT_TAG_TYPE_BOOTDEV 5
#define MULTIBOOT_TAG_TYPE_MMAP 6
#define MULTIBOOT_TAG_TYPE_VBE 7
#define MULTIBOOT_TAG_TYPE_FRAMEBUFFER 8
#define MULTIBOOT_TAG_TYPE_ELF_SECTIONS 9
#define MULTIBOOT_TAG_TYPE_APM 10
#define MULTIBOOT_TAG_TYPE_EFI32 11
#define MULTIBOOT_TAG_TYPE_EFI64 12
#define MULTIBOOT_TAG_TYPE_SMBIOS 13
#define MULTIBOOT_TAG_TYPE_ACPI_OLD 14
#define MULTIBOOT_TAG_TYPE_ACPI_NEW 15
#define MULTIBOOT_TAG_TYPE_NETWORK 16
#define MULTIBOOT_TAG_TYPE_EFI_MMAP 17
#define MULTIBOOT_TAG_TYPE_EFI_BS 18
#define MULTIBOOT_TAG_TYPE_EFI32_IH 19
#define MULTIBOOT_TAG_TYPE_EFI64_IH 20
#define MULTIBOOT_TAG_TYPE_LOAD_BASE_ADDR 21

#define MULTIBOOT_HEADER_TAG_END 0
#define MULTIBOOT_HEADER_TAG_INFORMATION_REQUEST 1
#define MULTIBOOT_HEADER_TAG_ADDRESS 2
#define MULTIBOOT_HEADER_TAG_ENTRY_ADDRESS 3
#define MULTIBOOT_HEADER_TAG_CONSOLE_FLAGS 4
#define MULTIBOOT_HEADER_TAG_FRAMEBUFFER 5
#define MULTIBOOT_HEADER_TAG_MODULE_ALIGN 6
#define MULTIBOOT_HEADER_TAG_EFI_BS 7
#define MULTIBOOT_HEADER_TAG_ENTRY_ADDRESS_EFI32 8
#define MULTIBOOT_HEADER_TAG_ENTRY_ADDRESS_EFI64 9
#define MULTIBOOT_HEADER_TAG_RELOCATABLE 10

#define MULTIBOOT_ARCHITECTURE_I386 0
#define MULTIBOOT_ARCHITECTURE_MIPS32 4
#define MULTIBOOT_HEADER_TAG_OPTIONAL 1

#define MULTIBOOT_LOAD_PREFERENCE_NONE 0
#define MULTIBOOT_LOAD_PREFERENCE_LOW 1
#define MULTIBOOT_LOAD_PREFERENCE_HIGH 2

#define MULTIBOOT_CONSOLE_FLAGS_CONSOLE_REQUIRED 1
#define MULTIBOOT_CONSOLE_FLAGS_EGA_TEXT_SUPPORTED 2



// 直接用 -m64 编译出来的是 64 位代码，
// 但是启动后的机器是 32 位的，相当于在 32 位机器上跑 64 位程序。
// 得加一层跳转到 64 位的 -m32 代码，开启 long 模式后再跳转到以 -m64 编译的代码中
// 对于 x86_64，需要在启动阶段进入长模式(IA32E)，这意味着需要一个临时页表
// See https://wiki.osdev.org/Creating_a_64-bit_kernel: 
// With a 32-bit bootstrap in your kernel

// 这部分是从保护模式启动 long 模式的代码
// 工作在 32bit
// 声明这一段代码以 32 位模式编译
.code32

// multiboot2 文件头
// 计算头长度
.SET HEADER_LENGTH, multiboot_header_end - multiboot_header
// 计算校验和
.SET CHECKSUM, -(MULTIBOOT2_HEADER_MAGIC + MULTIBOOT_ARCHITECTURE_I386 + HEADER_LENGTH)
// 8 字节对齐
.section .multiboot_header
.align MULTIBOOT_HEADER_ALIGN
// 声明所属段

multiboot_header:
    // 魔数
    .long MULTIBOOT2_HEADER_MAGIC
    // 架构
    .long MULTIBOOT_ARCHITECTURE_I386
    // 头长度
    .long HEADER_LENGTH
    // 校验和
    .long CHECKSUM
    // 添加其它内容在此，详细信息见 Multiboot2 Specification version 2.0.pdf

// 设置帧缓冲区
.align 8
framebuffer_tag_start:
    .short MULTIBOOT_HEADER_TAG_FRAMEBUFFER
    .short MULTIBOOT_HEADER_TAG_OPTIONAL
    .long framebuffer_tag_end - framebuffer_tag_start
    .long 1440
    .long 900
    .long 32
framebuffer_tag_end:
.align 8
	.short MULTIBOOT_HEADER_TAG_END
    // 结束标记
    .short 0
    .long 8
multiboot_header_end:

.section .bootstrap

.global _start
.type _start, @function
# 在 multiboot2.cpp 中定义

.extern _start64
.extern boot_info_addr
.extern multiboot2_magic
ENTRY(_start)
    // 关中断
    cli
    
    // multiboot2_info 结构体指针
    mov %ebx, mb2_info
    //mov %ebx, %e8
    // 魔数
    mov %eax, mb2_magic

    //mov %eax, %e9
    / 从保护模式跳转到长模式
    // 1. 允许 PAE
    mov %cr4, %eax
    or $(1<<5), %eax
    mov %eax, %cr4
    // 2. 设置临时页表
    // 最高级
    mov $pml4, %eax
    mov $pdpt, %ebx
    or $0x3, %ebx
    mov %ebx, 0(%eax)

    // 次级
    mov $pdpt, %eax
    mov $pd, %ebx
    or $0x3, %ebx
    mov %ebx, 0(%eax)

    // 次低级
    mov $pd, %eax
    mov $pt, %ebx
    or $0x3, %ebx
    mov %ebx, 0(%eax)

    // 最低级
    // 循环 512 次，填满一页
    mov $512, %ecx
    mov $pt, %eax
    mov $0x3, %ebx
.fill_pt:
    mov %ebx, 0(%eax)
    add $0x1000, %ebx
    add $8, %eax
    loop .fill_pt

.global enter_head_from_ap_boot
enter_head_from_ap_boot:
    // 填写 CR3
    mov $pml4, %eax
    mov %eax, %cr3

    // 3. 切换到 long 模式
    mov $0xC0000080, %ecx
    rdmsr
    or $(1<<8), %eax
    wrmsr

    // 4. 开启分页
    mov %cr0, %eax
    or $(1<<31), %eax
    mov %eax, %cr0

    // 5. 重新设置 GDT
    mov $gdt64_pointer, %eax
    lgdt 0(%eax)

    jmp $0x8, $ready_to_start_64
    hlt
    ret
.code64
.global ready_to_start_64
ready_to_start_64:

    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %fs
    mov %ax, %ss
    mov $0x7e00, %esp

    
    //6. 跳转到start64
    movq switch_to_start64(%rip), %rax
    pushq $0x08 //段选择子
    pushq %rax
    lretq

switch_to_start64:
    .quad _start64


.code64
is_from_ap:
    
    hlt

.global _start64
.type _start64, @function
.extern Start_Kernel
ENTRY(_start64)


    // 初始化寄存器
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %fs
    mov %ax, %ss
    mov $0x7e00, %esp
    
// === 加载GDTR ====
    lgdt GDT_POINTER(%rip) //这里我没搞明白rip相对寻址, 看了文档，大概是用来实现PIC的（position independent code）
    //lgdt $GDT_POINTER
// === 加载IDTR ====
    lidt IDT_POINTER(%rip)
    //lidt $IDT_POINTER
    movq GDT_POINTER(%rip), %r12
    movq head_stack_start(%rip), %rsp

    // 分支，判断是否为apu
    movq	$0x1b,	%rcx		// 根据IA32_APIC_BASE.BSP[8]标志位判断处理器是否为apu
	rdmsr
	bt	$8,	%rax
	jnc	load_cr3

    // 2. 设置临时页表
    // 最高级
    mov $__PML4E, %eax
    mov $__PDPTE, %ebx
    or $0x3, %ebx
    mov %ebx, 0(%eax)
    mov %ebx, 256(%eax)

    // 次级
    mov $__PDPTE, %eax
    mov $__PDE, %ebx
    or $0x3, %ebx
    mov %ebx, 0(%eax)


// ==== 加载CR3寄存器

load_cr3:

    movq $__PML4E, %rax //设置页目录基地址
    
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

    movq head_stack_start(%rip), %rsp //rsp的地址
    
    // 重新加载GDT和IDT，加载到高地址
    leaq GDT_Table(%rip), %r8
    leaq GDT_END(%rip), %r9

    subq %r8, %r9
    movq %r9, %r13    // GDT size

    leaq IDT_Table(%rip), %r8
    leaq IDT_END(%rip), %r9

    subq %r8, %r9
    movq %r9, %r12    // IDT size

    lgdt GDT_POINTER64(%rip)
    lidt IDT_POINTER64(%rip)

    // 分支，判断是否为apu
    movq	$0x1b,	%rcx		// 根据IA32_APIC_BASE.BSP[8]标志位判断处理器是否为apu
	rdmsr
	bt	$8,	%rax
	jnc	start_smp

setup_IDT:
    // 该部分代码只在启动初期使用，后面的c文件中会重新设置IDT，
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
    leaq TSS64_Table(%rip), %rdx    // 获取定义在process.c中的initial_tss[0]的地址

    movq $0xffff800000000000, %r8
    addq %r8, %rdx

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
    
    //now enable SSE and the like
    movq %cr0, %rax
    and $0xFFFB, %ax		//clear coprocessor emulation CR0.EM
    or $0x2, %ax			//set coprocessor monitoring  CR0.MP
    movq %rax, %cr0
    movq %cr4, %rax
    or $(3 << 9), %ax		//set CR4.OSFXSR and CR4.OSXMMEXCPT at the same time
    movq %rax, %cr4

    

    movq	go_to_kernel(%rip),	%rax		/* movq address */
	pushq	$0x08
	pushq	%rax

    

    movq mb2_info, %r15
    movq mb2_magic, %r14

    
	lretq

go_to_kernel:
    .quad Start_Kernel

start_smp:


    //now enable SSE and the like
    movq %cr0, %rax
    and $0xFFFB, %ax		//clear coprocessor emulation CR0.EM
    or $0x2, %ax			//set coprocessor monitoring  CR0.MP
    movq %rax, %cr0
    movq %cr4, %rax
    or $(3 << 9), %ax		//set CR4.OSFXSR and CR4.OSXMMEXCPT at the same time
    movq %rax, %cr4


	movq	go_to_smp_kernel(%rip),	%rax		/* movq address */
	pushq	$0x08
	pushq	%rax

/*
    // 重新加载GDT和IDT，加载到高地址
    leaq GDT_Table(%rip), %r8
    leaq GDT_END(%rip), %r9

    subq %r8, %r9
    movq %r9, %r13    // GDT size

    leaq IDT_Table(%rip), %r8
    leaq IDT_END(%rip), %r9

    subq %r8, %r9
    movq %r9, %r12    // IDT size

    lgdt GDT_POINTER64(%rip)
    lidt IDT_POINTER64(%rip)
*/
	lretq

go_to_smp_kernel:

	.quad	smp_ap_start

// ==== 异常/中断处理模块 ignore int： 忽略中断
// (该部分代码只在启动初期使用，后面的c文件中会重新设置IDT，从而重设ignore_int的中断入点)
m_ignore_int:
// 切换到c语言的ignore_int
    movq go_to_ignore_int(%rip), %rax
    pushq $0x08
    pushq %rax
    lretq



go_to_ignore_int:
    .quad ignore_int_handler


ENTRY(head_stack_start)
    .quad initial_proc_union + 32768


// 初始化页表
.align 0x1000 //设置为4k对齐
//.org 0x1000 //设置页表位置为内核执行头程序的0x1000处

__PML4E:
    .quad 0x103007 // 用户访问，可读写，已存在， 地址在31~12位
    .fill	255,8,0
	.quad 0x103003
	.fill	255,8,0

.org	0x2000

__PDPTE:

	.quad	0x104003 // 用户访问，可读写，已存在
	.fill	511,8,0

.org	0x3000

__PDE:

	.quad	0x000083    // 用户访问，可读写，已存在
	.quad	0x200083
	.quad	0x400083
	.quad	0x600083
	.quad	0x800083
    .quad	0xa00083
	.quad	0xc00083
	.quad	0xe00083
	.quad	0x1000083
	.quad	0x1200083
	.quad	0x1400083
	.quad	0x1600083
	.quad	0x1800083
	.quad	0x1a00083
	.quad	0x1c00083
	.quad	0x1e00083
	.quad	0x2000083
	.quad	0x2200083
	.quad	0x2400083
	.quad	0x2600083
	.quad	0x2800083
	.quad	0x2a00083
	.quad	0x2c00083
	.quad	0x2e00083
	.quad	0x3000083
	.quad	0x3200083
	.quad	0x3400083
	.quad	0x3600083


	.quad	0xe0000083		/*虚拟地址0x 3000000 初始情况下，帧缓冲区映射到这里*/ 
	.quad	0xe0200083
	.quad	0xe0400083
	.quad	0xe0600083		/*0x1000000*/
	.quad	0xe0800083
	.quad	0xe0a00083
	.quad	0xe0c00083
	.quad	0xe0e00083
    .quad	0xe1000083
    .quad	0xe1200083
    .quad	0xe1400083
    .quad	0xe1600083
    .quad	0xe1800083
    .quad	0xe1a00083
    .quad	0xe1c00083
    .quad	0xe1e00083
	.fill	468,8,0


// GDT表

.align 16
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
    .fill 100, 8, 0           // 10-11 TSS(跳过了第9段)  重复十次填充8字节的空间，赋值为0   长模式下，每个TSS长度为128bit
GDT_END:

.global GDT_POINTER
GDT_POINTER:
GDT_LIMIT: .word GDT_END - GDT_Table - 1 // GDT的大小
GDT_BASE: .quad GDT_Table

.global GDT_POINTER64
GDT_POINTER64:
GDT_LIMIT64: .word GDT_END - GDT_Table - 1 // GDT的大小
GDT_BASE64: .quad GDT_Table + 0xffff800000000000

// IDT 表
.global IDT_Table

IDT_Table:
    .fill 512, 8, 0 // 设置512*8字节的IDT表的空间
IDT_END:

.global IDT_POINTER
IDT_POINTER:
IDT_LIMIT: .word IDT_END - IDT_Table - 1
IDT_BASE: .quad IDT_Table

.global IDT_POINTER64
IDT_POINTER64:
IDT_LIMIT64: .word IDT_END - IDT_Table - 1
IDT_BASE64: .quad IDT_Table + 0xffff800000000000

// 64位的TSS表
.global TSS64_Table

TSS64_Table:
    .fill 13, 8, 0
TSS64_END:

.section .bootstrap.data
mb2_magic: .quad 0
mb2_info: .quad 0

.code32
// 临时页表 4KB/页
.align 0x1000
.global pml4
pml4:
    .skip 0x1000
pdpt:
    .skip 0x1000
pd:
    .skip 0x1000
pt:
    .skip 0x1000

// 临时 GDT
.align 16
gdt64:
null_desc:
    .short 0xFFFF
    .short 0
    .byte 0
    .byte 0
    .byte 0
    .byte 0
code_desc:
    .short 0
    .short 0
    .byte 0
    .byte 0x9A
    .byte 0x20
    .byte 0
data_desc:
    .short 0
    .short 0
    .byte 0
    .byte 0x92
    .byte 0
    .byte 0
user_code_desc:
    .short 0
    .short 0
    .byte 0
    .byte 0xFA
    .byte 0x20
    .byte 0
user_data_desc:
    .short 0
    .short 0
    .byte 0
    .byte 0xF2
    .byte 0
    .byte 0
gdt64_pointer:
    .short gdt64_pointer-gdt64-1
    .quad gdt64
gdt64_pointer64:
    .short gdt64_pointer-gdt64-1
    .quad gdt64

