#pragma once

#include "stddef.h"

// 操作系统定义时间以ns为单位
#define CLOCKS_PER_SEC 1000000


/**
 * @brief 获取当前的CPU时间
 *
 * @return uint64_t timer_jiffies
 */
extern uint64_t rs_clock();