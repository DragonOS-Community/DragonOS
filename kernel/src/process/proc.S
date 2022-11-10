#include"../common/asm.h"

/**
 * @brief 导出内核线程的执行引导程序
 *  目的是还原执行现场(在kernel_thread中伪造的)
 * 执行到这里时，rsp位于栈顶，然后弹出寄存器值
 * 弹出之后还要向上移动7个unsigned long的大小，从而弹出额外的信息（详见pt_regs）
 */
 
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
    addq	$0x20,	%rsp	   
    popfq
    addq    $0x10,  %rsp
    movq	%rdx,	%rdi  
    callq	*%rbx		   
    movq	%rax,	%rdi	   
    callq	process_do_exit		  