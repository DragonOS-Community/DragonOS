#pragma once

#include <common/glib.h>
#include <driver/timers/HPET/HPET.h>

// 定义LONG_MAX为最大超时时间 - 允许负数
#define MAX_TIMEOUT (int64_t)((1ul << 63) - 1)

extern void rs_timer_init();
extern int64_t rs_timer_get_first_expire();
extern uint64_t rs_timer_next_n_ms_jiffies(uint64_t expire_ms);
extern int64_t rs_schedule_timeout(int64_t timeout);

extern uint64_t rs_clock();