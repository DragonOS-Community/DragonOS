#pragma once

#define __force __attribute__((force))

#define likely(x) __builtin_expect(!!(x), 1)
#define unlikely(x) __builtin_expect(!!(x), 0)

#ifndef barrier
// 内存屏障
#define barrier() __asm__ __volatile__("" :: \
                                           : "memory");
#endif