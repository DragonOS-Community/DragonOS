#include "clocksource.h"
#include <common/kthread.h>

void run_watchdog_kthread()
{
    kerror("run_watchdog_kthread");
    while(1);
    // todo: 进程管理重构后，改掉这里，换到rust的实现
    // kthread_run(rs_clocksource_watchdog_kthread, NULL, "clocksource_watchdog");
}