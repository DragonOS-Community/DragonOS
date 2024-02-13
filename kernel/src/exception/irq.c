
#include "irq.h"
#include <common/errno.h>


#include <common/asm.h>
#include <common/printk.h>
#include <common/string.h>
#include <mm/slab.h>
#include <arch/arch.h>

#pragma GCC push_options
#pragma GCC optimize("O0")

/**
 * @brief 中断注册函数
 *
 * @param irq_num 中断向量号
 * @param arg 传递给中断安装接口的参数
 * @param handler 中断处理函数
 * @param paramater 中断处理函数的参数
 * @param controller 中断控制器结构
 * @param irq_name 中断名
 * @return int
 */
int irq_register(ul irq_num, void *arg, void (*handler)(ul irq_num, ul parameter, struct pt_regs *regs), ul paramater,
                 hardware_intr_controller *controller, char *irq_name)
{
    // 由于为I/O APIC分配的中断向量号是从32开始的，因此要减去32才是对应的interrupt_desc的元素
    irq_desc_t *p = NULL;
    if (irq_num >= 32 && irq_num < 0x80)
        p = &interrupt_desc[irq_num - 32];
    else if (irq_num >= 150 && irq_num < 200)
        p = &local_apic_interrupt_desc[irq_num - 150];
    else
    {
        kerror("irq_register(): invalid irq num: %ld.", irq_num);
        return -EINVAL;
    }
    p->controller = controller;
    if (p->irq_name == NULL)
    {
        int namelen = sizeof(strlen(irq_name) + 1);
        p->irq_name = (char *)kmalloc(namelen, 0);
        memset(p->irq_name, 0, namelen);
        strncpy(p->irq_name, irq_name, namelen);
    }

    p->parameter = paramater;
    p->flags = 0;
    p->handler = handler;
    io_mfence();
    p->controller->install(irq_num, arg);
    io_mfence();
    p->controller->enable(irq_num);
    io_mfence();

    return 0;
}

/**
 * @brief 中断注销函数
 *
 * @param irq_num 中断向量号
 * @return int
 */
int irq_unregister(ul irq_num)
{
    irq_desc_t *p = &interrupt_desc[irq_num - 32];
    p->controller->disable(irq_num);
    p->controller->uninstall(irq_num);

    p->controller = NULL;
    if (p->irq_name)
        kfree(p->irq_name);
    p->irq_name = NULL;
    p->parameter = (ul)NULL;
    p->flags = 0;
    p->handler = NULL;

    return 0;
}

#pragma GCC optimize("O0")
