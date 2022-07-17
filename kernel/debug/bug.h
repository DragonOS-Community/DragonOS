#pragma once
#include <common/compiler.h>
#include <common/kprint.h>

/**
 * @brief 当condition为true时输出警告信息
 *
 */
#define WARN_ON(condition) ({                                   \
    int __ret_warn_on = !!(condition);                          \
    if (unlikely(__ret_warn_on))                                \
        kwarn("Assertion failed at %s:%d", __FILE__, __LINE__); \
    unlikely(__ret_warn_on);                                    \
})
