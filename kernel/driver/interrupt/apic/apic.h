#pragma once

#include "../../../common/asm.h"
#include "../../../process/ptrace.h"
#include "../../../exception/irq.h"
#include "../../../mm/mm.h"

#define APIC_IO_APIC_VIRT_BASE_ADDR SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + IO_APIC_MAPPING_OFFSET
#define APIC_LOCAL_APIC_VIRT_BASE_ADDR SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + LOCAL_APIC_MAPPING_OFFSET

// ======== local apic 寄存器地址偏移量表 =======
// 0x00~0x10 Reserved.
#define LOCAL_APIC_OFFSET_Local_APIC_ID 0x20
#define LOCAL_APIC_OFFSET_Local_APIC_Version 0x30
// 0x40~0x70 Reserved.
#define LOCAL_APIC_OFFSET_Local_APIC_TPR 0x80
#define LOCAL_APIC_OFFSET_Local_APIC_APR 0x90
#define LOCAL_APIC_OFFSET_Local_APIC_PPR 0xa0
#define LOCAL_APIC_OFFSET_Local_APIC_EOI 0xb0
#define LOCAL_APIC_OFFSET_Local_APIC_RRD 0xc0
#define LOCAL_APIC_OFFSET_Local_APIC_LDR 0xd0
#define LOCAL_APIC_OFFSET_Local_APIC_DFR 0xe0
#define LOCAL_APIC_OFFSET_Local_APIC_SVR 0xf0

#define LOCAL_APIC_OFFSET_Local_APIC_ISR_31_0 0x100
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_63_32 0x110
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_95_64 0x120
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_127_96 0x130
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_159_128 0x140
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_191_160 0x150
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_223_192 0x160
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_255_224 0x170

#define LOCAL_APIC_OFFSET_Local_APIC_TMR_31_0 0x180
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_63_32 0x190
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_95_64 0x1a0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_127_96 0x1b0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_159_128 0x1c0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_191_160 0x1d0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_223_192 0x1e0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_255_224 0x1f0

#define LOCAL_APIC_OFFSET_Local_APIC_IRR_31_0 0x200
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_63_32 0x210
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_95_64 0x220
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_127_96 0x230
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_159_128 0x240
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_191_160 0x250
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_223_192 0x260
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_255_224 0x270

#define LOCAL_APIC_OFFSET_Local_APIC_ESR 0x280

// 0x290~0x2e0 Reserved.

#define LOCAL_APIC_OFFSET_Local_APIC_LVT_CMCI 0x2f0
#define LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0 0x300
#define LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32 0x310
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER 0x320
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_THERMAL 0x330
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_PERFORMANCE_MONITOR 0x340
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0 0x350
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1 0x360
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR 0x370
// 初始计数寄存器（定时器专用）
#define LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG 0x380
// 当前计数寄存器（定时器专用）
#define LOCAL_APIC_OFFSET_Local_APIC_CURRENT_COUNT_REG 0x390
// 0x3A0~0x3D0 Reserved.
// 分频配置寄存器（定时器专用）
#define LOCAL_APIC_OFFSET_Local_APIC_CLKDIV 0x3e0





struct apic_IO_APIC_map
{
    // 间接访问寄存器的物理基地址
    uint addr_phys;
    // 索引寄存器虚拟地址
    unsigned char *virtual_index_addr;
    // 数据寄存器虚拟地址
    uint *virtual_data_addr;
    // EOI寄存器虚拟地址
    uint *virtual_EOI_addr;
} apic_ioapic_map;

/**
 * @brief 中断服务程序
 *
 * @param rsp 中断栈指针
 * @param number 中断号
 */
void do_IRQ(struct pt_regs *rsp, ul number);

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