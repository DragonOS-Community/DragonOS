#include "syscall.h"
#include <libc/src/include/stdio.h>
#include <libc/src/include/errno.h>
long syscall_invoke(uint64_t syscall_id, uint64_t arg0, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5, uint64_t arg6, uint64_t arg7)
{
    uint64_t __err_code;
    __asm__ __volatile__(
        "movq %2, %%r8 \n\t"
        "movq %3, %%r9 \n\t"
        "movq %4, %%r10 \n\t"
        "movq %5, %%r11 \n\t"
        "movq %6, %%r12 \n\t"
        "movq %7, %%r13 \n\t"
        "movq %8, %%r14 \n\t"
        "movq %9, %%r15 \n\t"
        "int $0x80   \n\t"
        "movq %%rax, %0 \n\t"
        :"=a"(__err_code)
        : "a"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2), "m"(arg3), "m"(arg4), "m"(arg5), "m"(arg6), "m"(arg7)
        : "memory", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rcx", "rdx");
    // printf("errcode = %#018lx\n", __err_code);
    errno = __err_code;
    
    return __err_code;
}
