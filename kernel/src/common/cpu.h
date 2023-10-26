#pragma once

#include "glib.h"

#define MAX_CPU_NUM 32 // 操作系统支持的最大处理器数量

// cpu支持的最大cpuid指令的基础主功能号
extern uint32_t Cpu_cpuid_max_Basic_mop;
// cpu支持的最大cpuid指令的扩展主功能号
extern uint32_t Cpu_cpuid_max_Extended_mop;
// cpu制造商信息
extern char Cpu_Manufacturer_Name[17];
// 处理器名称信息
extern char Cpu_BrandName[49];
// 处理器家族ID
extern uint32_t Cpu_Family_ID;
// 处理器扩展家族ID
extern uint32_t Cpu_Extended_Family_ID;
// 处理器模式ID
extern uint32_t Cpu_Model_ID;
// 处理器扩展模式ID
extern uint32_t Cpu_Extended_Model_ID;
// 处理器步进ID
extern uint32_t Cpu_Stepping_ID;
// 处理器类型
extern uint32_t Cpu_Processor_Type;
// 处理器支持的最大物理地址可寻址地址线宽度
extern uint32_t Cpu_max_phys_addrline_size;
// 处理器支持的最大线性地址可寻址地址线宽度
extern uint32_t Cpu_max_linear_addrline_size;

// 处理器的tsc频率（单位：hz）(HPET定时器在测定apic频率时，顺便测定了这个值)
extern uint64_t Cpu_tsc_freq;

/**
 * @brief 执行cpuid指令
 *
 * @param mop 主功能号
 * @param sop 子功能号
 * @param eax 结果的eax值
 * @param ebx 结果的ebx值
 * @param ecx 结果的ecx值
 * @param edx 结果的edx值
 *
 * cpuid指令参考英特尔开发手册卷2A Chapter3 3.2 Instruction
 */
void cpu_cpuid(uint32_t mop, uint32_t sop, uint32_t *eax, uint32_t *ebx, uint32_t *ecx, uint32_t *edx);

/**
 * @brief 初始化获取处理器信息模块
 *
 */
void cpu_init(void);

struct cpu_core_info_t
{
    uint64_t stack_start;     // 栈基地址
    uint64_t ist_stack_start; // IST栈基地址
};

extern struct cpu_core_info_t cpu_core_info[MAX_CPU_NUM];
