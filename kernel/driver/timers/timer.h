#pragma once

#include <common/glib.h>
#include "HPET/HPET.h"
#include "rtc/rtc.h"

uint64_t volatile timer_jiffies = 0;   // 系统时钟计数

void timer_init();

void do_timer_softirq(void* data);