#pragma once


extern int rs_clocksource_watchdog_kthread(void *_data);
extern void rs_clocksource_boot_finish();

void run_watchdog_kthread();
