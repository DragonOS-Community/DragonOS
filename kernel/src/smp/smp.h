#pragma once
#include <common/glib.h>
#include <common/stddef.h>
#include <common/asm.h>

#define MAX_SUPPORTED_PROCESSOR_NUM 1024    



extern uchar _apu_boot_start[];
extern uchar _apu_boot_end[];
/**
 * @brief 初始化对称多核处理器
 *
 */
void smp_init();

extern int64_t rs_kick_cpu(uint32_t cpu_id);

uint32_t smp_get_total_cpu();

extern void set_current_core_tss(uint64_t stack_start, uint64_t ist0);
extern void load_current_core_tss();