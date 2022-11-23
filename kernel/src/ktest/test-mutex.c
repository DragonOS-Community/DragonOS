#include "ktest_utils.h"
#include <common/mutex.h>
#include <common/time.h>
#include <common/sys/wait.h>
#include <process/process.h>

static mutex_t mtx;

/**
 * @brief 测试是否能够加锁
 *
 * @param arg0
 * @param arg1
 * @return long
 */
static long ktest_mutex_case0(uint64_t arg0, uint64_t arg1)
{
    assert(mutex_is_locked(&mtx) == 0);
    mutex_lock(&mtx);
    assert(mutex_is_locked(&mtx) == 1);
    mutex_unlock(&mtx);
    assert(mutex_is_locked(&mtx) == 0);
    assert(mutex_trylock(&mtx) == 1);
    mutex_unlock(&mtx);
    assert(mutex_is_locked(&mtx) == 0);
}

/**
 * @brief 测试用例1的辅助线程
 *
 * @param arg
 * @return long
 */
static int ktest_mutex_case1_pid1(void* arg)
{
    kTEST("ktest_mutex_case1_subproc start.");
    assert(mutex_is_locked(&mtx) == 1);
    mutex_lock(&mtx);
    assert(atomic_read(&mtx.count) == 0);
    assert(list_empty(&mtx.wait_list));

    mutex_unlock(&mtx);
    kTEST("ktest_mutex_case1_subproc exit.");
    return 0;
}

static long ktest_mutex_case1(uint64_t arg0, uint64_t arg1)
{
    if (!assert(mutex_is_locked(&mtx) == 0))
        goto failed;

    // 加锁
    mutex_lock(&mtx);
    // 启动另一个线程
    pid_t pid = kernel_thread(ktest_mutex_case1_pid1, 0, 0);
    // 等待100ms
    usleep(100000);
    while (list_empty(&mtx.wait_list))
        ;

    // 当子线程加锁后，计数应当为0
    assert(atomic_read(&mtx.count) == 0);
    struct mutex_waiter_t *wt = container_of(list_next(&mtx.wait_list), struct mutex_waiter_t, list);
    assert(wt->pcb->pid == pid);

    mutex_unlock(&mtx);

    int stat = 1;
    waitpid(pid, &stat, 0);
    assert(stat == 0);
    return 0;
failed:;
    kTEST("mutex test case1 failed.");
    return -1;
}

static ktest_case_table kt_mutex_func_table[] = {
    ktest_mutex_case0,
    ktest_mutex_case1,
};
int ktest_test_mutex(void* arg)
{
    kTEST("Testing mutex...");
    mutex_init(&mtx);

    for (int i = 0; i < sizeof(kt_mutex_func_table) / sizeof(ktest_case_table); ++i)
    {
        kTEST("Testing case %d", i);
        kt_mutex_func_table[i](i, 0);
    }
    kTEST("mutex Test done.");
    return 0;
}