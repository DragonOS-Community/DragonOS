#include <io/scheduler.h>
#include <common/kthread.h>
/**
 * @brief 初始化io调度器
 */
void io_scheduler_init()
{
    io_scheduler_init_rust();
    kthread_run(&address_requests, NULL, "io_scheduler", NULL);
}