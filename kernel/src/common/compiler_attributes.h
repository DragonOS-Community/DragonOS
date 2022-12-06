#pragma once
#include <common/stddef.h>

// 当函数的返回值未被使用时，编译器抛出警告信息
#define __must_check __attribute__((__warn_unused_result__))
#define __force __attribute__((force))
// 无返回值的属性
#define __noreturn __attribute__((__noreturn__))
/*
 * Optional: only supported since clang >= 14.0
 *
 *   gcc: https://gcc.gnu.org/onlinedocs/gcc/Common-Function-Attributes.html#index-error-function-attribute
 */
#if __has_attribute(__error__)
#define __compiletime_error(msg) __attribute__((__error__(msg)))
#else
#define __compiletime_error(msg)
#endif

typedef uint8_t __attribute__((__may_alias__)) __u8_alias_t;
typedef uint16_t __attribute__((__may_alias__)) __u16_alias_t;
typedef uint32_t __attribute__((__may_alias__)) __u32_alias_t;
typedef uint64_t __attribute__((__may_alias__)) __u64_alias_t;