#pragma once

#include <common/glib.h>
#include <process/ptrace.h>
#include <common/time.h>


/**
 * @brief 休眠指定时间
 * 
 * @param rqtp 指定休眠的时间
 * @param rmtp 返回的剩余休眠时间
 * @return int 
 */
int nanosleep(const struct timespec *rqtp, struct timespec *rmtp);

/**
 * @brief 睡眠指定时间
 *
 * @param usec 微秒
 * @return int
 */
int usleep(useconds_t usec);