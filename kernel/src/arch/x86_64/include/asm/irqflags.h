#pragma once
#include <asm/asm.h>
// 保存当前rflags的值到变量x内并关闭中断
#define local_irq_save(x) __asm__ __volatile__("pushfq ; popq %0 ; cli" \
                                               : "=g"(x)::"memory")
// 恢复先前保存的rflags的值x
#define local_irq_restore(x) __asm__ __volatile__("pushq %0 ; popfq" ::"g"(x) \
                                                  : "memory")
#define local_irq_disable() cli();
#define local_irq_enable() sti();
