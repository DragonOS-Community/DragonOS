#include <clocksource.h>
void run_watchdog_kthread()
{
    kdebug("run_watchdog_kthread");
    kthread_run(rs_clocksource_watchdog_kthread, NULL, "clocksource_watchdog");
}