#pragma once

#include <common/glib.h>
#include "HPET/HPET.h"
#include "rtc/rtc.h"

uint64_t volatile timer_jiffies = 0;   // 系统时钟计数

void timer_init();

void do_timer_softirq(void* data);

/**
 * @brief 定时功能队列
 * 
 */
struct timer_func_list_t
{
    struct List list;
    uint64_t expire_jiffies;
    void (*func)(void* data);
    void* data;
}timer_func_head;   

/**
 * @brief 初始化定时功能
 * 
 * @param timer_func 队列结构体
 * @param func 定时功能处理函数
 * @param data 传输的数据
 * @param expire_ms 定时时长(单位：ms)
 */
void timer_func_init(struct timer_func_list_t * timer_func, void (*func)(void*data), void*data,uint64_t expire_ms);

/**
 * @brief 将定时功能添加到列表中
 * 
 * @param timer_func 待添加的定时功能
 */
void timer_func_add(struct timer_func_list_t* timer_func);

/**
 * @brief 将定时功能从列表中删除
 * 
 * @param timer_func 
 */
void timer_func_del(struct timer_func_list_t* timer_func);


