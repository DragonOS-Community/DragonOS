#pragma once
#include <common/compiler.h>
#include <common/kprint.h>

#pragma GCC push_options
#pragma GCC optimize("O0")

/**
 * @brief 当condition为true时，认为产生了bug
 *
 */
#define BUG_ON(condition) ({                      \
    int __ret_bug_on = !!(condition);             \
    if (unlikely(__ret_bug_on))                   \
        kBUG("BUG at %s:%d", __FILE__, __LINE__); \
    unlikely(__ret_bug_on);                       \
})

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

/**
 * @brief 当condition不为0时输出警告信息，且只会输出一次警告信息
 * 
 */
#define WARN_ON_ONCE(condition) ({              \
    static int __warned;                        \
    int __ret_warn_once = !!(condition);        \
                                                \
    if (unlikely(__ret_warn_once && !__warned)) \
    {                                           \
        __warned = true;                        \
        WARN_ON(1);                             \
    }                                           \
    unlikely(__ret_warn_once);                  \
})

#define FAIL_ON_TO(condition, to) ({   \
    int __ret_warn_on = !!(condition); \
    if (unlikely(__ret_warn_on))       \
        goto to;                       \
    unlikely(__ret_warn_on);           \
})
#pragma GCC pop_options