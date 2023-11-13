#pragma once

#include <common/glib.h>


// 定义LONG_MAX为最大超时时间 - 允许负数
#define MAX_TIMEOUT (int64_t)((1ul << 63) - 1)

extern void rs_timer_init();

extern void rs_jiffies_init();
