#pragma once

#include <common/unistd.h>
#include "apic.h"

#define APIC_TIMER_IRQ_NUM 151

/**
 * @brief 初始化local APIC定时器
 *
 */
void apic_timer_init();

void apic_timer_ap_core_init();
