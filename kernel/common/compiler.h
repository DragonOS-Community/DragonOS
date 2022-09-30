#pragma once

#define __force __attribute__((force))

#define likely(x) __builtin_expect(!!(x), 1)
#define unlikely(x) __builtin_expect(!!(x), 0)

#ifndef barrier
// 内存屏障
#define barrier() __asm__ __volatile__("" :: \
                                           : "memory");
#endif

// 编译器属性

// 当函数的返回值未被使用时，编译器抛出警告信息
#define __must_check __attribute__((__warn_unused_result__))