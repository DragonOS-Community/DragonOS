#pragma once
#include <common/glib.h>
struct rtc_time_t
{
    int second;
    int minute;
    int hour;
    int day;
    int month;
    int year;
}rtc_now;   // rtc_now为墙上时钟，由HPET定时器0维护

/**
 * @brief 从主板cmos中获取时间
 * 
 * @param t time结构体
 * @return int 成功则为0
 */
int rtc_get_cmos_time(struct rtc_time_t*t);

void rtc_init();