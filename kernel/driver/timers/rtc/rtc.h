#pragma once
#include <common/glib.h>
struct time
{
    int second;
    int minute;
    int hour;
    int day;
    int month;
    int year;
};

/**
 * @brief 从主板cmos中获取时间
 * 
 * @param t time结构体
 * @return int 成功则为0
 */
int rtc_get_cmos_time(struct time*t);
int get_cmos_time(struct time *time);