#include <common/asm.h>
.code64
//.section .text

R15 =   0x00
R14 =   0x08
R13 =   0x10
R12 =   0x18
R11 =   0x20
R10 =   0x28
R9  =   0x30
R8  =   0x38
RBX =   0x40
RCX =   0x48
RDX =   0x50
RSI =   0x58
RDI	=	0x60
RBP	=	0x68
DS	=	0x70
ES	=	0x78
RAX	=	0x80
FUNC	=	0x88
ERRCODE	=	0x90
// 以下几个字段，在中断产生时，由处理器压入栈内
RIP	    =	0x98
CS	    =	0xa0
RFLAGS	=	0xa8
OLD_RSP	=	0xb0
OLDSS	=	0xb8

Restore_all:
    // === 恢复调用现场 ===
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbx
    popq %rcx
    popq %rdx
    popq %rsi
    popq %rdi
    popq %rbp

    popq %rax   //  不允许直接pop到ds
    movq %rax, %ds

    popq %rax
    movq %rax, %es

    popq %rax
    addq $0x10, %rsp // 弹出变量FUNC和errcode
    
    sti
    iretq

ret_from_exception:
    // === 从中断中返回 ===

ENTRY(ret_from_intr)

    // 进入信号处理流程
    cli

    // 将原本要返回的栈帧的栈指针传入do_signal的第一个参数
    movq %rsp, %rdi
    callq do_signal
    cli

__entry_ret_from_intr_before_gs_check_2:
    push %rcx
    addq $8, %rsp
    movq CS(%rsp), %rcx
    subq $8, %rsp
    andq $0x3, %rcx
    cmpq $0x3, %rcx

    jne __entry_ret_from_intr_after_gs_check_2
    swapgs

__entry_ret_from_intr_after_gs_check_2:
    popq %rcx

    // 恢复寄存器
    jmp Restore_all

    
Err_Code:

    // ===== 有错误码的情况下，保存寄存器并跳转服务程序
    pushq	%rax
	movq	%es,	%rax
	pushq	%rax
	movq	%ds,	%rax
	pushq	%rax
	xorq	%rax,	%rax

	pushq	%rbp
	pushq	%rdi
	pushq	%rsi
	pushq	%rdx
	pushq	%rcx
	pushq	%rbx
	pushq	%r8
	pushq	%r9
	pushq	%r10
	pushq	%r11
	pushq	%r12
	pushq	%r13
	pushq	%r14
	pushq	%r15

    cld
    
    movq ERRCODE(%rsp), %rsi    // 把错误码装进rsi，作为函数的第二个参数
    movq FUNC(%rsp), %rdx
    
    movq $0x10, %rdi    // 加载内核段的地址
    movq %rdi, %ds
    movq %rdi, %es

    movq %rsp, %rdi // 把栈指针装入rdi，作为函数的第一个的参数

__entry_err_code_before_gs_check_1:
    pushq %rcx
    movq CS(%rdi), %rcx
    and $0x3, %rcx
    cmp $0x3, %rcx

    jne __entry_err_code_after_gs_check_1
    swapgs

__entry_err_code_after_gs_check_1:
    popq %rcx

    callq *%rdx //调用服务程序 带*号表示调用的是绝对地址

__entry_err_code_to_ret_from_exception:
    jmp ret_from_exception


// 0 #DE 除法错误
ENTRY(trap_divide_error)
    
    pushq $0    //由于#DE不会产生错误码，但是为了保持弹出结构的一致性，故也压入一个错误码0
    pushq %rax  // 先将rax入栈
    leaq do_divide_error(%rip), %rax    // 获取中断服务程序的地址
    
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 1 #DB 调试异常
ENTRY(trap_debug)
    pushq $0    
    pushq %rax  
    leaq do_debug(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 2 不可屏蔽中断
ENTRY(trap_nmi)
    // 不可屏蔽中断不是异常，而是一个外部中断，不会产生错误码
    // 应执行中断处理流程
    pushq $0  //占位err_code

    pushq %rax
    leaq do_nmi(%rip), %rax
    xchgq %rax, (%rsp)
    jmp Err_Code

// 3 #BP 断点异常
ENTRY(trap_int3)
    pushq $0
    pushq %rax
    leaq do_int3(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 4 #OF 溢出异常
ENTRY(trap_overflow)
    pushq $0
    pushq %rax
    leaq do_overflow(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 5 #BR 越界异常
ENTRY(trap_bounds)
    pushq $0
    pushq %rax
    leaq do_bounds(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 6 #UD 无效/未定义的机器码
ENTRY(trap_undefined_opcode)
    pushq $0
    pushq %rax
    leaq do_undefined_opcode(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 7 #NM 设备异常（FPU不存在）
ENTRY(trap_dev_not_avaliable)
    pushq $0
    pushq %rax
    leaq do_dev_not_avaliable(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 8 #DF 双重错误
ENTRY(trap_double_fault)
    pushq %rax
    leaq do_double_fault(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 9 协处理器越界（保留）
ENTRY(trap_coprocessor_segment_overrun)
    pushq $0
    pushq %rax
    leaq do_coprocessor_segment_overrun(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 10 #TS 无效的TSS段
ENTRY(trap_invalid_TSS)
    // === 不正确的任务状态段 #TS ==
    // 有错误码,处理器已经自动在异常处理程序栈中压入错误码
    pushq %rax
    leaq do_invalid_TSS(%rip), %rax
    xchgq %rax, (%rsp)
    jmp Err_Code

// 11 #NP 段不存在
ENTRY(trap_segment_not_exists)
    pushq %rax
    leaq do_segment_not_exists(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 12 #SS 段错误
ENTRY(trap_stack_segment_fault)
    pushq %rax
    leaq do_stack_segment_fault(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 13 #GP 通用保护性异常
ENTRY(trap_general_protection)
    pushq %rax
    leaq do_general_protection(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 14 #PF 页错误
ENTRY(trap_page_fault)
    // === 页故障 #PF ==
    // 有错误码
    pushq %rax
    leaq do_page_fault(%rip), %rax
    xchgq %rax, (%rsp)
    jmp Err_Code

// 15 Intel保留，请勿使用

// 16 #MF X87 FPU错误（计算错误）
ENTRY(trap_x87_FPU_error)
    pushq $0
    pushq %rax
    leaq do_x87_FPU_error(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 17 #AC 对齐检测
ENTRY(trap_alignment_check)
    pushq %rax
    leaq do_alignment_check(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 18 #MC 机器检测
ENTRY(trap_machine_check)
    pushq $0
    pushq %rax
    leaq do_machine_check(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 19 #XM SIMD浮点异常
ENTRY(trap_SIMD_exception)
    pushq $0
    pushq %rax
    leaq do_SIMD_exception(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// 20 #VE 虚拟化异常
ENTRY(trap_virtualization_exception)
    pushq $0
    pushq %rax
    leaq do_virtualization_exception(%rip), %rax    // 获取中断服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code



// 系统调用入口
// 0x80 系统调用门
ENTRY(syscall_int)
    pushq $0
    pushq %rax
    leaq syscall_handler(%rip), %rax    // 获取系统调用服务程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

// irq模块初始化后的ignore_int入点
ENTRY(ignore_int)
    pushq $0
    pushq %rax
    leaq ignore_int_handler(%rip), %rax    // 获取ignore处理程序的地址
    xchgq %rax, (%rsp)  // 把FUNC的地址换入栈中
    jmp Err_Code

ENTRY(syscall_64)
    // 切换用户栈和内核栈
    cli
    swapgs
    movq %rsp, %gs:0x8
    movq %gs:0x0, %rsp
    
    pushq $43       // USER_DS
    pushq %gs:0x8   // rsp
    pushq %r11      // RFLAGS
    pushq $51       // USER_CS
    pushq %rcx      // RIP
    pushq $0        // error code占位

    pushq %rax
    leaq syscall_handler(%rip), %rax // FUNC
    xchgq %rax, (%rsp)

    pushq %rax      // rax
    
    movq %es, %rax
    pushq %rax      // es
    movq %ds, %rax
    pushq %rax      // ds
    xorq %rax, %rax

    pushq	%rbp
	pushq	%rdi
	pushq	%rsi
	pushq	%rdx
	pushq	%rcx
	pushq	%rbx
	pushq	%r8
	pushq	%r9
	pushq	%r10
	pushq	%r11
	pushq	%r12
	pushq	%r13
	pushq	%r14
	pushq	%r15

    cld

    xorq %rsi, %rsi
    movq FUNC(%rsp), %rdx

    movq %rsp, %rdi // 把栈指针装入rdi，作为函数的第一个的参数

    sti
    callq *%rdx //调用服务程序

    // 将原本要返回的栈帧的栈指针传入do_signal的第一个参数
    movq %rsp, %rdi

    callq do_signal

    cli
    
    // === 恢复调用现场 ===
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbx
    popq %rcx
    popq %rdx
    popq %rsi
    popq %rdi
    popq %rbp

    popq %rax           // 不允许直接pop到ds
    movq %rax, %ds

    popq %rax
    movq %rax, %es

    popq %rax
    addq $0x10, %rsp    // 弹出变量FUNC和errcode

    popq %rcx           // pop rip到rcx

    addq $0x8, %rsp     // 弹出cs
    popq %r11           // pop rflags到r11
    popq %rsp           // Restore rsp

    swapgs
    sysretq
