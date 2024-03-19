#include "syscall.h"
#include <arch/arch.h>
#include <common/errno.h>
#include <common/fcntl.h>
#include <common/string.h>
#include <mm/slab.h>
#include <process/process.h>
#include <time/sleep.h>

#if ARCH(I386) || ARCH(X86_64)
// 导出系统调用入口函数，定义在entry.S中
extern void syscall_int(void);

/**
 * @brief 通过中断进入系统调用
 *
 * @param syscall_id
 * @param arg0
 * @param arg1
 * @param arg2
 * @param arg3
 * @param arg4
 * @param arg5
 * @param arg6
 * @param arg7
 * @return long
 */

long enter_syscall_int(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3,
                       ul arg4, ul arg5) {
  long err_code;
  __asm__ __volatile__("movq %2, %%rdi \n\t"
                       "movq %3, %%rsi \n\t"
                       "movq %4, %%rdx \n\t"
                       "movq %5, %%r10 \n\t"
                       "movq %6, %%r8 \n\t"
                       "movq %7, %%r9 \n\t"
                       "int $0x80   \n\t"
                       : "=a"(err_code)
                       : "a"(syscall_id), "m"(arg0), "m"(arg1), "m"(arg2),
                         "m"(arg3), "m"(arg4), "m"(arg5)
                       : "memory", "r8", "r9", "r10", "rdi", "rsi", "rdx");

  return err_code;
}

#else
long enter_syscall_int(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3,
                       ul arg4, ul arg5) {
  while (1) {
    /* code */
  }
}

#endif
