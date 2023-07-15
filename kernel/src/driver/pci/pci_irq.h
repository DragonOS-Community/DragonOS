#pragma once
#include <common/glib.h>
#include <process/ptrace.h>
uint16_t c_irq_install(ul irq_num,void (*pci_irq_handler)(ul irq_num, ul parameter, struct pt_regs *regs),ul parameter,const char *irq_name,void (*pci_irq_ack)(ul irq_num));
void c_irq_uninstall(ul irq_num);