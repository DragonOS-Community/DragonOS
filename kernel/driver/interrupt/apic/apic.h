#pragma once

#include "../../../common/asm.h"
#include"../../../process/ptrace.h"
#include"../../../exception/irq.h"
#include "../../../mm/mm.h"

#define APIC_IO_APIC_VIRT_BASE_ADDR SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + IO_APIC_MAPPING_OFFSET
#define APIC_LOCAL_APIC_VIRT_BASE_ADDR SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + LOCAL_APIC_MAPPING_OFFSET

// ======== local apic 寄存器地址偏移量表 =======
#define LOCAL_APIC_OFFSET_Local_APIC_ID 0x20
#define LOCAL_APIC_OFFSET_Local_APIC_Version 0x30
#define LOCAL_APIC_OFFSET_Local_APIC_TPR 0x80
#define LOCAL_APIC_OFFSET_Local_APIC_APR 0x90
#define LOCAL_APIC_OFFSET_Local_APIC_PPR 0xa0
#define LOCAL_APIC_OFFSET_Local_APIC_EOI 0xb0
#define LOCAL_APIC_OFFSET_Local_APIC_RRD 0xc0
#define LOCAL_APIC_OFFSET_Local_APIC_LDR 0xd0
#define LOCAL_APIC_OFFSET_Local_APIC_DFR 0xe0

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
 * @brief 读取RTE寄存器
 * 
 * @param index 索引值
 * @return ul 
 */
ul apic_ioapic_read_rte(unsigned char index);

/**
 * @brief 写入RTE寄存器
 * 
 * @param index 索引值
 * @param value 要写入的值
 */
void apic_ioapic_write_rte(unsigned char index, ul value);

/**
 * @brief 初始化apic控制器
 * 
 */
void apic_init();