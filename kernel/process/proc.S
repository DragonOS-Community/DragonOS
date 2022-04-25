#include"../common/asm.h"

ENTRY(kernel_thread_func) 
    popq	%r15	   
    popq	%r14	   
    popq	%r13	   
    popq	%r12	   
    popq	%r11	   
    popq	%r10	   
    popq	%r9	   
    popq	%r8	   
    popq	%rbx	   
    popq	%rcx	   
    popq	%rdx	   
    popq	%rsi	   
    popq	%rdi	   
    popq	%rbp	   
    popq	%rax	   
    movq	%rax,	%ds	   
    popq	%rax		   
    movq	%rax,	%es	   
    popq	%rax		   
    addq	$0x38,	%rsp	   
    movq	%rdx,	%rdi	   
    callq	*%rbx		   
    movq	%rax,	%rdi	   
    callq	process_thread_do_exit		  