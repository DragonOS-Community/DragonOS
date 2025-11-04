/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * relocate_kernel_64.S - put the kernel image in place to boot
 * Copyright (C) Jingxuan Wei  <jensenwei007@gmail.com>
 */

#define X86_CR4_PGE		(1ul << 7)
#define X86_CR4_CET		(1ul << 23)
#define X86_CR0_AM          (1UL<<18)
#define X86_CR0_WP          (1UL<<16)
#define X86_CR0_TS          (1UL<<3)
#define X86_CR0_EM          (1UL<<2)
#define X86_CR0_PG          (1UL<<31)
#define X86_CR0_PE          (1UL<<0)
#define X86_CR4_PAE		(1ul << 5)
#define X86_CR4_LA57		(1ul << 12)

.section .relocate_kernel_data
.balign 16
.globl kexec_pa_table_page
kexec_pa_table_page:
    .quad 0
    .type kexec_pa_table_page 1
    .set .L__sym_size_kexec_pa_table_page, .-kexec_pa_table_page
    .size kexec_pa_table_page, .L__sym_size_kexec_pa_table_page

.section .relocate_kernel_text
.code64
.globl relocate_kernel;   
relocate_kernel:
	/*
	 * %rdi indirection_page
	 * %rsi start_address
	 * %rdx stack_page_address
	 */

	/* zero out flags, and disable interrupts */
	pushq $0
	popfq

    /* Switch to the identity mapped page tables */
	movq	%cr3, %rax
	movq	kexec_pa_table_page(%rip), %r9
	movq	%r9, %cr3

    /* Leave CR4 in %r13 to enable the right paging mode later. */
	movq	%cr4, %r13

	/* Disable global pages immediately to ensure this mapping is RWX */
	movq	%r13, %r12
	andq	$~(X86_CR4_PGE), %r12
	movq	%r12, %cr4

    /* setup a new stack */
	movq	%rdx, %rsp
    addq    $4096, %rsp

    /* store the start address on the stack */
	pushq   %rsi

    /*
	 * Clear X86_CR4_CET (if it was set) such that we can clear CR0_WP
	 * below.
	 */
	movq	%cr4, %rax
	andq	$~(X86_CR4_CET), %rax
	movq	%rax, %cr4

    /*
	 * Set cr0 to a known state:
	 *  - Paging enabled
	 *  - Alignment check disabled
	 *  - Write protect disabled
	 *  - No task switch
	 *  - Don't do FP software emulation.
	 *  - Protected mode enabled
	 */
	movq	%cr0, %rax
	andq	$~(X86_CR0_AM | X86_CR0_WP | X86_CR0_TS | X86_CR0_EM), %rax
	orl	$(X86_CR0_PG | X86_CR0_PE), %eax
	movq	%rax, %cr0

    /*
	 * Set cr4 to a known state:
	 *  - physical address extension enabled
	 *  - 5-level paging, if it was enabled before
	 *  - Machine check exception on TDX guest, if it was enabled before.
	 *    Clearing MCE might not be allowed in TDX guests, depending on setup.
	 *
	 * Use R13 that contains the original CR4 value, read in relocate_kernel().
	 * PAE is always set in the original CR4.
	 */
	andl	$(X86_CR4_PAE | X86_CR4_LA57), %r13d
	movq	%r13, %cr4

    /* Flush the TLB (needed?) */
	movq	%r9, %cr3

    call	swap_pages

    /*
	 * To be certain of avoiding problems with self-modifying code
	 * I need to execute a serializing instruction here.
	 * So I flush the TLB by reloading %cr3 here, it's handy,
	 * and not processor dependent.
	 */
	movq	%cr3, %rax
	movq	%rax, %cr3

	/*
	 * set all of the registers to known values
	 * leave %rsp alone
	 */

	xorl	%eax, %eax
	xorl	%ebx, %ebx
	xorl    %ecx, %ecx
	xorl    %edx, %edx
	xorl    %esi, %esi
	xorl    %edi, %edi
	xorl    %ebp, %ebp
	xorl	%r8d, %r8d
	xorl	%r9d, %r9d
	xorl	%r10d, %r10d
	xorl	%r11d, %r11d
	xorl	%r12d, %r12d
	xorl	%r13d, %r13d
	xorl	%r14d, %r14d
	xorl	%r15d, %r15d

    ret
	int3

.type relocate_kernel 0 ;
.set .L__sym_size_relocate_kernel, .-relocate_kernel ;
.size relocate_kernel, .L__sym_size_relocate_kernel

/* Do the copies */
swap_pages:
	/*
	 * %rdi indirection page
	 */
    xor %rax, %rax # wjx

	movq	%rdi, %rcx	/* Put the indirection_page in %rcx */
	xorl	%edi, %edi
	xorl	%esi, %esi
	jmp	.Lstart		/* Should start with an indirection record */

.Lloop:	/* top, read another word for the indirection page */

	movq	(%rbx), %rcx
	addq	$8,	%rbx
.Lstart:
	testb	$0x1,	%cl   /* is it a destination page? */
	jz	.Lnotdest
	movq	%rcx,	%rdi
	andq	$0xfffffffffffff000, %rdi
	jmp	.Lloop
.Lnotdest:
	testb	$0x2,	%cl   /* is it an indirection page? */
	jz	.Lnotind
	movq	%rcx,   %rbx
	andq	$0xfffffffffffff000, %rbx
    #inc %rax #wjx
    #cmp $15, %rax
    #jne	.Lloop
    #je .Lwjx
    jmp	.Lloop
.Lnotind:
	testb	$0x4,	%cl   /* is it the done indicator? */
	jz	.Lnotdone
	jmp	.Ldone
.Lnotdone:
	testb	$0x8,	%cl   /* is it the source indicator? */
	jz	.Lloop	      /* Ignore it otherwise */

	movq	%rcx,   %rsi  /* For ever source page do a copy */
	andq	$0xfffffffffffff000, %rsi

	movl	$512, %ecx
	rep ; movsq

	jmp	.Lloop
.Ldone:
	ret
	int3
.Lwjx:
    jmp .
.type swap_pages 0 ;
.set .L__sym_size_swap_pages, .-swap_pages ;
.size swap_pages, .L__sym_size_swap_pages