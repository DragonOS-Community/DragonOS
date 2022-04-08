#pragma once

#include <common/glib.h>
#include <driver/acpi/acpi.h>
#include<driver/timers/rtc/rtc.h>

#define E_HPET_INIT_FAILED 1
int HPET_init();