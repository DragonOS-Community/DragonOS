#include "timer.h"
#include <common/kprint.h>
#include <exception/softirq.h>
#include <mm/slab.h>
#include <driver/timers/HPET/HPET.h>
#include <process/process.h>

struct timer_func_list_t timer_func_head;

// 定时器循环阈值，每次最大执行10个定时器任务
#define TIMER_RUN_CYCLE_THRESHOLD 10

void test_timer()
{
    printk_color(ORANGE, BLACK, "(test_timer)");
}

void timer_init()
{
    timer_jiffies = 0;
    timer_func_init(&timer_func_head, NULL, NULL, -1UL);
    register_softirq(TIMER_SIRQ, &do_timer_softirq, NULL);

    struct timer_func_list_t *tmp = (struct timer_func_list_t *)kmalloc(sizeof(struct timer_func_list_t), 0);
    timer_func_init(tmp, &test_timer, NULL, 5);
    timer_func_add(tmp);

    kdebug("timer func initialized.");
}

void do_timer_softirq(void *data)
{
    // todo: 修改这里以及softirq的部分，使得timer具有并行性
    struct timer_func_list_t *tmp = container_of(list_next(&timer_func_head.list), struct timer_func_list_t, list);
    int cycle_count = 0;
    while ((!list_empty(&timer_func_head.list)) && (tmp->expire_jiffies <= timer_jiffies))
    {

        timer_func_del(tmp);
        tmp->func(tmp->data);
        kfree(tmp);

        ++cycle_count;
        // 当前定时器达到阈值
        if (cycle_count == TIMER_RUN_CYCLE_THRESHOLD)
            break;
        tmp = container_of(list_next(&timer_func_head.list), struct timer_func_list_t, list);
    }
}

/**
 * @brief 初始化定时功能
 *
 * @param timer_func 队列结构体
 * @param func 定时功能处理函数
 * @param data 传输的数据
 * @param expire_ms 定时时长(单位：ms)
 */
void timer_func_init(struct timer_func_list_t *timer_func, void (*func)(void *data), void *data, uint64_t expire_ms)
{
    list_init(&timer_func->list);
    timer_func->func = func;
    timer_func->data = data;
    timer_func->expire_jiffies = cal_next_n_ms_jiffies(expire_ms); // 设置过期的时间片
}

/**
 * @brief 初始化定时功能
 *
 * @param timer_func 队列结构体
 * @param func 定时功能处理函数
 * @param data 传输的数据
 * @param expire_us 定时时长(单位：us)
 */
void timer_func_init_us(struct timer_func_list_t *timer_func, void (*func)(void *data), void *data, uint64_t expire_us)
{
    list_init(&timer_func->list);
    timer_func->func = func;
    timer_func->data = data;
    timer_func->expire_jiffies = cal_next_n_us_jiffies(expire_us); // 设置过期的时间片
    // kdebug("timer_func->expire_jiffies=%ld",cal_next_n_us_jiffies(expire_us));
}

/**
 * @brief 将定时功能添加到列表中
 *
 * @param timer_func 待添加的定时功能
 */
void timer_func_add(struct timer_func_list_t *timer_func)
{
    struct timer_func_list_t *tmp = container_of(list_next(&timer_func_head.list), struct timer_func_list_t, list);

    if (list_empty(&timer_func_head.list) == false)
        while (tmp->expire_jiffies < timer_func->expire_jiffies)
            tmp = container_of(list_next(&tmp->list), struct timer_func_list_t, list);

    list_add(&tmp->list, &(timer_func->list));
}

/**
 * @brief 将定时功能从列表中删除
 *
 * @param timer_func
 */
void timer_func_del(struct timer_func_list_t *timer_func)
{
    list_del(&timer_func->list);
}

uint64_t sys_clock(struct pt_regs *regs)
{
    return timer_jiffies;
}

uint64_t clock()
{
    return timer_jiffies;
}
