#pragma once

// 保存当前rflags的值到变量x内并关闭中断
#define local_irq_save(x) \
    do                    \
    {                     \
    } while (1)
// 恢复先前保存的rflags的值x
#define local_irq_restore(x) \
    do                       \
    {                        \
    } while (1)
#define local_irq_disable() cli();
#define local_irq_enable() sti();
