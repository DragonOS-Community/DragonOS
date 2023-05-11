#include <kthread.h>

extern int rs_clocksource_watchdog_kthread(void *_data);
extern void rs_clocksource_init();
void run_watchdog_kthread()
{
    kdebug("run_watchdog_kthread");
    kthread_run(rs_clocksource_watchdog_kthread, NULL, "clocksource_watchdog");
}