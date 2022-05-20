#pragma once

#include <common/glib.h>
#include <driver/acpi/acpi.h>
#include<driver/timers/rtc/rtc.h>

#define E_HPET_INIT_FAILED 1

#define HPET0_INTERVAL 5    // HPET0定时器的中断间隔为5ms
int HPET_init();