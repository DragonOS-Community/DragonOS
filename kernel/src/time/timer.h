#pragma once

#include <common/glib.h>
#include <driver/timers/HPET/HPET.h>
#include <driver/timers/rtc/rtc.h>

// 定义LONG_MAX为最大超时时间 - 允许负数
#define MAX_TIMEOUT (int64_t)((1ul << 63) - 1)

uint64_t volatile timer_jiffies = 0; // 系统时钟计数

// 计算接下来n毫秒对应的系统时间片
#define cal_next_n_ms_jiffies(expire_ms) (timer_jiffies + 1000 * (expire_ms))
// 计算接下来n微秒对应的系统时间片
#define cal_next_n_us_jiffies(expire_us) (timer_jiffies + (expire_us))

void timer_init();

void do_timer_softirq(void *data);

/**
 * @brief 定时功能队列
 *
 */
struct timer_func_list_t
{
    struct List list;
    uint64_t expire_jiffies;
    void (*func)(void *data);
    void *data;
};

extern struct timer_func_list_t timer_func_head;
/**
 * @brief 初始化定时功能
 *
 * @param timer_func 队列结构体
 * @param func 定时功能处理函数
 * @param data 传输的数据
 * @param expire_ms 定时时长(单位：ms)
 */
void timer_func_init(struct timer_func_list_t *timer_func, void (*func)(void *data), void *data, uint64_t expire_ms);

/**
 * @brief 初始化定时功能
 *
 * @param timer_func 队列结构体
 * @param func 定时功能处理函数
 * @param data 传输的数据
 * @param expire_us 定时时长(单位：us)
 */
void timer_func_init_us(struct timer_func_list_t *timer_func, void (*func)(void *data), void *data, uint64_t expire_us);

/**
 * @brief 将定时功能添加到列表中
 *
 * @param timer_func 待添加的定时功能
 */
void timer_func_add(struct timer_func_list_t *timer_func);

/**
 * @brief 将定时功能从列表中删除
 *
 * @param timer_func
 */
void timer_func_del(struct timer_func_list_t *timer_func);

uint64_t clock();

/**
 * @brief 睡眠timeout的时间之后唤醒进程/线程
 * 
 * @param timeout 
 * @return long 
 */
long schedule_timeout_ms(long timeout);