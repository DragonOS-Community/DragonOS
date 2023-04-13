#pragma once
#include <common/glib.h>

#include <common/asm.h>

#define MAX_SUPPORTED_PROCESSOR_NUM 1024    



extern uchar _apu_boot_start[];
extern uchar _apu_boot_end[];
/**
 * @brief 初始化对称多核处理器
 *
 */
void smp_init();

int kick_cpu(uint32_t cpu_id);

uint32_t smp_get_total_cpu();