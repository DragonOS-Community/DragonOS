#pragma once

#include "../../../common/asm.h"
#include"../../../process/ptrace.h"
#include"../../../exception/irq.h"

struct apic_IO_APIC_map
{
    // 间接访问寄存器的物理基地址
    uint addr_phys;
    // 索引寄存器虚拟地址
    unsigned char* virtual_index_addr;
    // 数据寄存器虚拟地址
    uint* virtual_data_addr;
    // EOI寄存器虚拟地址
    uint* virtual_EOI_addr;
}apic_ioapic_map;

/**
 * @brief 中断服务程序
 * 
 * @param rsp 中断栈指针
 * @param number 中断号
 */
void do_IRQ(struct pt_regs* rsp, ul number);

/**
 * @brief 初始化apic控制器
 * 
 */
void apic_init();