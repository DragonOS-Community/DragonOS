#pragma once
#include <common/sys/types.h>

uint64_t ktest_test_bitree(uint64_t arg);
uint64_t ktest_test_kfifo(uint64_t arg);
uint64_t ktest_test_mutex(uint64_t arg);
uint64_t ktest_test_idr(uint64_t arg);

/**
 * @brief 开启一个新的内核线程以进行测试
 *
 * @param func 测试函数
 * @param arg 传递给测试函数的参数
 * @return pid_t 测试内核线程的pid
 */
pid_t ktest_start(uint64_t (*func)(uint64_t arg), uint64_t arg);