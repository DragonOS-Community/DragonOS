#pragma once

#include <common/glib.h>
#include <common/time.h>
#include <process/ptrace.h>

/**
 * @brief 睡眠指定时间
 *
 * @param usec 微秒
 * @return int
 */
int rs_usleep(useconds_t usec);
