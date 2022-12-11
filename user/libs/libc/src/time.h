#pragma once

#include "stddef.h"

// 操作系统定义时间以ns为单位
#define CLOCKS_PER_SEC 1000000

struct tm
{
    int tm_sec;   /* Seconds.	[0-60] (1 leap second) */
    int tm_min;   /* Minutes.	[0-59] */
    int tm_hour;  /* Hours.	[0-23] */
    int tm_mday;  /* Day.		[1-31] */
    int tm_mon;   /* Month.	[0-11] */
    int tm_year;  /* Year	- 1900.  */
    int tm_wday;  /* Day of week.	[0-6] */
    int tm_yday;  /* Days in year.[0-365]	*/
    int tm_isdst; /* DST.		[-1/0/1]*/

    long int __tm_gmtoff;  /* Seconds east of UTC.  */
    const char *__tm_zone; /* Timezone abbreviation.  */
};


struct timespec
{
    long int tv_sec;    // 秒
    long int tv_nsec;   // 纳秒
};

/**
 * @brief 休眠指定时间
 * 
 * @param rqtp 指定休眠的时间
 * @param rmtp 返回的剩余休眠时间
 * @return int 
 */
int nanosleep(const struct timespec *rqtp, struct timespec *rmtp);

/**
 * @brief 获取系统当前cpu时间
 * 
 * @return clock_t 
 */
clock_t clock();