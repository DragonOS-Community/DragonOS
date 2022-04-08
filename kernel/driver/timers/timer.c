#include "timer.h"
#include<common/kprint.h>
#include <exception/softirq.h>

void timer_init()
{
    timer_jiffies = 0;
    register_softirq(0, &do_timer_softirq, NULL);
}

void do_timer_softirq(void* data)
{
    printk_color(ORANGE, BLACK, "(HPET%ld)", timer_jiffies);
}