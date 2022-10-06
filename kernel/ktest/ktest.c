#include "ktest.h"
#include <process/process.h>

/**
 * @brief 开启一个新的内核线程以进行测试
 *
 * @param func 测试函数
 * @param arg 传递给测试函数的参数
 * @return pid_t 测试内核线程的pid
 */
pid_t ktest_start(int (*func)(void* arg), void* arg)
{
    return kernel_thread(func, arg, 0);
}