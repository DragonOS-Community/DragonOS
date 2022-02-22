#pragma once

#include "glib.h"

#define CPU_NUM 8 

// cpu支持的最大cpuid指令的基础主功能号
uint Cpu_cpuid_max_Basic_mop;
// cpu支持的最大cpuid指令的扩展主功能号
uint Cpu_cpuid_max_Extended_mop;
// cpu制造商信息
char Cpu_Manufacturer_Name[17]={0};
// 处理器名称信息
char Cpu_BrandName[49] = {0};
// 处理器家族ID
uint Cpu_Family_ID;
// 处理器扩展家族ID
uint Cpu_Extended_Family_ID;
// 处理器模式ID
uint Cpu_Model_ID;
// 处理器扩展模式ID
uint Cpu_Extended_Model_ID;
// 处理器步进ID
uint Cpu_Stepping_ID;
// 处理器类型
uint Cpu_Processor_Type;
// 处理器支持的最大物理地址可寻址地址线宽度
uint Cpu_max_phys_addrline_size;
// 处理器支持的最大线性地址可寻址地址线宽度
uint Cpu_max_linear_addrline_size;

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
void cpu_cpuid(uint mop, uint sop, uint *eax, uint*ebx, uint*ecx, uint*edx)
{   
    // 向eax和ecx分别输入主功能号和子功能号
    // 结果输出到eax, ebx, ecx, edx
    __asm__ __volatile__("cpuid \n\t":"=a"(*eax),"=b"(*ebx), "=c"(*ecx), "=d"(*edx):"0"(mop),"2"(sop):"memory");
}

/**
 * @brief 初始化获取处理器信息模块
 * 
 */
void cpu_init(void);
