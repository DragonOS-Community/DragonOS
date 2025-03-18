#include "common/asm.h"

#define __VCPU_REGS_RAX  0
#define __VCPU_REGS_RCX  1
#define __VCPU_REGS_RDX  2
#define __VCPU_REGS_RBX  3
#define __VCPU_REGS_RSP  4
#define __VCPU_REGS_RBP  5
#define __VCPU_REGS_RSI  6
#define __VCPU_REGS_RDI  7

#define __VCPU_REGS_R8   8
#define __VCPU_REGS_R9   9
#define __VCPU_REGS_R10 10
#define __VCPU_REGS_R11 11
#define __VCPU_REGS_R12 12
#define __VCPU_REGS_R13 13
#define __VCPU_REGS_R14 14
#define __VCPU_REGS_R15 15

#define VCPU_RAX __VCPU_REGS_RAX * 8
#define VCPU_RCX __VCPU_REGS_RCX * 8
#define VCPU_RDX __VCPU_REGS_RDX * 8
#define VCPU_RBX __VCPU_REGS_RBX * 8
#define VCPU_RBP __VCPU_REGS_RBP * 8
#define VCPU_RSI __VCPU_REGS_RSI * 8
#define VCPU_RDI __VCPU_REGS_RDI * 8

#define VCPU_R8  __VCPU_REGS_R8  * 8
#define VCPU_R9  __VCPU_REGS_R9  * 8
#define VCPU_R10 __VCPU_REGS_R10 * 8
#define VCPU_R11 __VCPU_REGS_R11 * 8
#define VCPU_R12 __VCPU_REGS_R12 * 8
#define VCPU_R13 __VCPU_REGS_R13 * 8
#define VCPU_R14 __VCPU_REGS_R14 * 8
#define VCPU_R15 __VCPU_REGS_R15 * 8

#define VMX_RUN_VMRESUME_SHIFT		0
#define VMX_RUN_SAVE_SPEC_CTRL_SHIFT	1

#define VMX_RUN_VMRESUME		1 << VMX_RUN_VMRESUME_SHIFT
#define VMX_RUN_SAVE_SPEC_CTRL		1 << VMX_RUN_SAVE_SPEC_CTRL_SHIFT

// 将VCPU运行在guest模式
ENTRY(__vmx_vcpu_run)
    pushq %rbp
    movq %rsp, %rbp

    pushq %r15
    pushq %r14
    pushq %r13
    pushq %r12

    push %rbx

    // 参数一
    push %rdi
    // 参数三
    push %rdx
    // 参数二
    push %rsi

    mov %edx, %ebx

    lea (%rsp), %rsi

    call vmx_update_host_rsp

    // TODO: spec_ctrl

.Lspec_ctrl_done:
    mov %rsp, %rax

    bt $VMX_RUN_VMRESUME_SHIFT, %ebx

    mov VCPU_RCX(%rax), %rcx
    mov VCPU_RDX(%rax), %rdx
    mov VCPU_RBX(%rax), %rbx
    mov VCPU_RBP(%rax), %rbp
    mov VCPU_RSI(%rax), %rsi
    mov VCPU_RDI(%rax), %rdi

    mov VCPU_R8(%rax), %R8
    mov VCPU_R9(%rax), %r9
    mov VCPU_R10(%rax), %r10
    mov VCPU_R11(%rax), %r11
    mov VCPU_R12(%rax), %r12
    mov VCPU_R13(%rax), %r13
    mov VCPU_R14(%rax), %r14
    mov VCPU_R15(%rax), %r15

    mov VCPU_RAX(%rax), %rax

    // TODO: clear cpu buffer

    jnc .Lvmlaunch

.Lvmresume:
    vmresume
    jmp .Lvmfail

.Lvmlaunch:
    call vmx_vmlaunch
    jmp .Lvmfail

// 从guest模式退出
ENTRY(vmx_vmexit)
    // TODO: unwind hint restore
    // 临时保存guest RAX
    push %rax

    // 拿到regs头指针，存入rax
    mov 8(%rsp), %rax

    // 保存所有guest寄存器
    pop VCPU_RAX(%rax)
    mov %rcx, VCPU_RCX(%rax)
    mov %rdx, VCPU_RDX(%rax)
    mov %rbx, VCPU_RBX(%rax)
    mov %rbp, VCPU_RBP(%rax)
    mov %rsi, VCPU_RSI(%rax)
    mov %rdi, VCPU_RDI(%rax)

    mov %r8, VCPU_R8(%rax)
    mov %r9, VCPU_R9(%rax)
    mov %r10, VCPU_R10(%rax)
    mov %r11, VCPU_R11(%rax)
    mov %r12, VCPU_R12(%rax)
    mov %r13, VCPU_R13(%rax)
    mov %r14, VCPU_R14(%rax)
    mov %r15, VCPU_R15(%rax)

    xor %ebx, %ebx

.Lclear_regs:
    pop %rax

    xor %eax, %eax
    xor %ecx, %ecx
    xor %edx, %edx
    xor %ebp, %ebp
    xor %esi, %esi
    xor %edi, %edi

    xor %r8d, %r8d
    xor %r9d, %r9d
    xor %r10d, %r10d
    xor %r11d, %r11d
    xor %r12d, %r12d
    xor %r13d, %r13d
    xor %r14d, %r14d
    xor %r15d, %r15d

    // todo: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmenter.S#270

    pop %rsi
    pop %rdi

    call vmx_spec_ctrl_restore_host

    mov %rbx, %rax

    pop %rbx

    pop %r12
    pop %r13
    pop %r14
    pop %r15

    pop %rbp
    ret

.Lvmfail:
    // 失败，设置返回值为1
    mov $1, %rbx
    jmp .Lclear_regs



