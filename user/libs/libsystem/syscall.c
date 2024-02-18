#include "syscall.h"
#include <stdio.h>
#include <errno.h>
long syscall_invoke(uint64_t syscall_id, uint64_t arg0, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5)
{
    uint64_t __err_code;
    __asm__ __volatile__(
        "movq %2, %%rdi \n\t"
        "movq %3, %%rsi \n\t"
        "movq %4, %%rdx \n\t"
        "movq %5, %%r10 \n\t"
        "movq %6, %%r8 \n\t"
        "movq %7, %%r9 \n\t"
        "int $0x80   \n\t"
        "movq %%rax, %0 \n\t"
        :"=a"(__err_code)
        : "a"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2), "m"(arg3), "m"(arg4), "m"(arg5)
        : "memory", "r8", "r9", "r10", "r11", "rcx", "rdx", "rdi", "rsi");
    // printf("errcode = %#018lx\n", __err_code);
    errno = __err_code;
    
    return __err_code;
}
