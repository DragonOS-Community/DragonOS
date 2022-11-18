#pragma once

#include <common/glib.h>
#include <driver/acpi/acpi.h>
#include <driver/timers/rtc/rtc.h>

#define E_HPET_INIT_FAILED 1

#define HPET0_INTERVAL 500 // HPET0定时器的中断间隔为500us
int HPET_init();

/**
 * @brief 测定apic定时器以及tsc的频率
 *
 */
void HPET_measure_freq();

/**
 * @brief 启用HPET周期中断（5ms）
 *
 */
void HPET_enable();