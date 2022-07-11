#include "timer.h"
#include <common/kprint.h>
#include <exception/softirq.h>
#include <mm/slab.h>
#include <driver/timers/HPET/HPET.h>
#include <process/process.h>

struct timer_func_list_t timer_func_head;

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
    
    struct timer_func_list_t *tmp = container_of(list_next(&timer_func_head.list), struct timer_func_list_t, list);

    while ((!list_empty(&timer_func_head.list)) && (tmp->expire_jiffies <= timer_jiffies))
    {
        
        timer_func_del(tmp);
        tmp->func(tmp->data);
        kfree(tmp);
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
    timer_func->data = data,
    // timer_func->expire_jiffies = timer_jiffies + expire_ms / 5 + expire_ms % HPET0_INTERVAL ? 1 : 0; // 设置过期的时间片
    timer_func->expire_jiffies = cal_next_n_ms_jiffies(expire_ms); // 设置过期的时间片
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