#include "../common/asm.h"


.balign 0x1000  // 按照4k对齐

.text
.code16

ENTRY(_apu_boot_start)
_apu_boot_base = .
    cli
    wbinvd  // 将处理器缓存同步到内存中

    mov %cs, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    mov %ax, %fs
    mov %ax, %gs

    // 设置栈指针
    movl $(_apu_boot_tmp_stack_end - _apu_boot_base), %esp
    
    // 计算ap处理器引导程序的基地址
    mov %cs, %ax
    movzx %ax, %esi
    shll $4, %esi


    // set gdt and 32bit/64bit code addr

    leal (_apu_code32 - _apu_boot_base)(%esi), %eax
    movl %eax, (_apu_code32_vector - _apu_boot_base)

    leal (_apu_code64 - _apu_boot_base)(%esi), %eax
    movl %eax, (_apu_code64_vector - _apu_boot_base)

    leal (_apu_tmp_gdt - _apu_boot_base)(%esi), %eax
    movl %eax, (_apu_tmp_gdt + 2 - _apu_boot_base)
    
// 从实模式切换到保护模式

    lidtl _apu_tmp_idt - _apu_boot_base
    lgdtl _apu_tmp_gdt - _apu_boot_base

    // 操作cr0控制器，使能保护模式
    smsw %ax
    bts $0, %ax
    lmsw %ax

    // 转到保护模式
    ljmpl *(_apu_code32_vector - _apu_boot_base)


.code32
.balign 4
_apu_code32:
    # 转到长模式

    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    mov %ax, %fs
    mov %ax, %gs

    // 设置栈指针
    leal (_apu_boot_tmp_stack_end - _apu_boot_base)(%esi), %eax
    movl %eax, %esp

    // open PAE
    movl %cr4, %eax
    bts $5, %eax
    movl %eax, %cr4

    // 设置页表

    movl $pml4, %eax    // 复用bsp处理器初始化时的32位页表
    movl %eax, %cr3

    mov $0xC0000080, %ecx
    rdmsr
    or $(1<<8), %eax
    wrmsr

    // enable PE and paging 这里有问题

    mov %cr0, %eax
    or $(1<<31), %eax
    mov %eax, %cr0

    // 跳转到64位代码
    ljmp *(_apu_code64_vector - _apu_boot_base)(%esi)


.code64
.balign 4
_apu_code64:
    movq $0x20, %rax
    movq %rax, %ds
    movq %rax, %es
    movq %rax, %ss
    movq %rax, %fs
    movq %rax, %gs

    //now enable SSE and the like
    movq %cr0, %rax
    and $0xFFFB, %ax		//clear coprocessor emulation CR0.EM
    or $0x2, %ax			//set coprocessor monitoring  CR0.MP
    movq %rax, %cr0
    movq %cr4, %rax
    or $(3 << 9), %ax		//set CR4.OSFXSR and CR4.OSXMMEXCPT at the same time
    movq %rax, %cr4

    // 跳转到地址1MB处执行
    movq	$_start64,	%rax
	jmpq	*%rax

    hlt


.balign 4
_apu_tmp_idt:
	.word	0
	.word	0,0

.balign 4
_apu_tmp_gdt:
    .short _apu_tmp_gdt_end - _apu_tmp_gdt -1
    .long _apu_tmp_gdt - _apu_boot_base
    .short 0
    .quad	0x00cf9a000000ffff
	.quad	0x00cf92000000ffff
	.quad	0x0020980000000000
	.quad	0x0000920000000000
_apu_tmp_gdt_end:

.balign 4
_apu_code32_vector:
	.long	_apu_code32 - _apu_boot_base
	.word	0x08,0	

.balign 4
_apu_code64_vector:
	.long	_apu_code64 - _apu_boot_base
	.word	0x18,0	

.balign 4
_apu_boot_tmp_stack_start:
	.org	0x400
_apu_boot_tmp_stack_end:

ENTRY(_apu_boot_end)

