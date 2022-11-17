#pragma once

#include <common/printk.h>
#include <common/compiler.h>

#define assert(condition) ({                                                                       \
    int __condition = !!(condition);                                                               \
    if (unlikely(!(__condition)))                                                                  \
    {                                                                                              \
        printk("[ kTEST FAILED ] Ktest Assertion Failed, file:%s, Line:%d\n", __FILE__, __LINE__); \
    }                                                                                              \
    likely(__condition);                                                                           \
})

#define kTEST(...)                                                  \
    do                                                              \
    {                                                               \
        printk("[ kTEST ] file:%s, Line:%d\t", __FILE__, __LINE__); \
        printk(__VA_ARGS__);                                        \
        printk("\n");                                               \
    } while (0)

/**
 * @brief 测试用例函数表
 *
 */
typedef long (*ktest_case_table)(uint64_t arg0, uint64_t arg1);