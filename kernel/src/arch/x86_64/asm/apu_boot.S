#include "../common/asm.h"
#include <asm/apu_boot.h>


.align 0x1000  // 按照4k对齐

.section .text
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
.align 0x1000 
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

    // 1. 允许 PAE
    mov %cr4, %eax
    or $(1<<5), %eax
    mov %eax, %cr4

    movl $enter_head_from_ap_boot, %eax
    jmpl	*%eax
    hlt

    

.code64
.align 0x1000 
_apu_code64:

    hlt


.align 0x1000 
_apu_tmp_idt:
	.word	0
	.word	0,0

.align 0x1000 
_apu_tmp_gdt:
    .short _apu_tmp_gdt_end - _apu_tmp_gdt -1
    .long _apu_tmp_gdt - _apu_boot_base
    .short 0
    .quad	0x00cf9a000000ffff
	.quad	0x00cf92000000ffff
	.quad	0x0020980000000000
	.quad	0x0000920000000000
_apu_tmp_gdt_end:

.align 0x1000 
_apu_code32_vector:
	.long	_apu_code32 - _apu_boot_base
	.word	0x08,0	

.align 0x1000 
_apu_code64_vector:
	.long	_apu_code64 - _apu_boot_base
	.word	0x18,0	

.align 0x1000
ENTRY(_apu_boot_tmp_stack_start)
	.skip	APU_BOOT_TMP_STACK_SIZE
_apu_boot_tmp_stack_end:

ENTRY(_apu_boot_end)

