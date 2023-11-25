#include "pci_irq.h"
#include "exception/irq.h"
#include <common/errno.h>
#include <common/kprint.h>
#include "common/string.h"
#include "mm/slab.h"

// 现在pci设备的中断由自己进行控制，这些不执行内容的函数是为了适配旧的中断处理机制
void pci_irq_enable(ul irq_num)
{
}
void pci_irq_disable(ul irq_num)
{
}
ul pci_irq_install(ul num , void* data)
{
}
void pci_irq_uninstall(ul irq_num)
{
}
/// @brief 与本操作系统的中断机制进行交互，把中断处理函数等注册到中断结构体中（被rust调用）
/// @param irq_num 要进行注册的中断号
/// @param pci_irq_handler 对应的中断处理函数
/// @param parameter 中断处理函数传入参数
/// @param irq_name 中断名字
/// @param pci_irq_ack 对于中断的回复，为NULL时会使用默认回应
uint16_t c_irq_install(ul irq_num, void (*pci_irq_handler)(ul irq_num, ul parameter, struct pt_regs *regs), ul parameter, const char *irq_name, void (*pci_irq_ack)(ul irq_num))
{
    // 由于为I/O APIC分配的中断向量号是从32开始的，因此要减去32才是对应的interrupt_desc的元素
    irq_desc_t *p = NULL;
    hardware_intr_controller *pci_interrupt_controller = NULL;
    if (irq_num >= 32 && irq_num < 0x80)
        p = &interrupt_desc[irq_num - 32];
    else if (irq_num >= 150 && irq_num < 200)
        p = &local_apic_interrupt_desc[irq_num - 150];
    else
    {
        // kerror("irq install for pci irq: invalid irq num: %ld.", irq_num);
        return EINVAL;
    }
    if (p->irq_name != NULL)
    {
        return EAGAIN;
    }
    pci_interrupt_controller = kzalloc(sizeof(hardware_intr_controller), 0);
    if (pci_interrupt_controller)
    {
        pci_interrupt_controller->enable = pci_irq_enable;
        pci_interrupt_controller->disable = pci_irq_disable;
        pci_interrupt_controller->install = pci_irq_install;
        pci_interrupt_controller->uninstall = pci_irq_uninstall;
        pci_interrupt_controller->ack = pci_irq_ack;
        p->controller = pci_interrupt_controller;
    }
    else
    {
        return EAGAIN;
    }
    size_t namelen = strlen(irq_name) + 1;
    p->irq_name = (char *)kzalloc(namelen, 0);
    memset(p->irq_name, 0, namelen);
    strncpy(p->irq_name, irq_name, namelen);
    p->parameter = parameter;
    p->flags = 0;
    p->handler = pci_irq_handler;
    return 0;
};

/// @brief 与本操作系统的中断机制进行交互，把中断处理函数等从中断结构体中移除，需要释放空间的进行空间的释放
/// @param irq_num 要进行注销的中断号
void c_irq_uninstall(ul irq_num)
{
    // 由于为I/O APIC分配的中断向量号是从32开始的，因此要减去32才是对应的interrupt_desc的元素
    irq_desc_t *p = NULL;
    if (irq_num >= 32 && irq_num < 0x80)
        p = &interrupt_desc[irq_num - 32];
    else if (irq_num >= 150 && irq_num < 200)
        p = &local_apic_interrupt_desc[irq_num - 150];
    else
    {
        kerror("irq install for pci irq: invalid irq num: %ld.", irq_num);
    }
    if (p->irq_name != NULL)
    {
        kfree(p->irq_name);
        p->irq_name = NULL;
    }
    if (p->controller != NULL)
    {
        kfree(p->controller);
        p->controller = NULL;
    }
    p->parameter = 0;
    p->handler = NULL;
}