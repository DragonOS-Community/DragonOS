/**
 * @file kprint.h
 * @author longjin
 * @brief 内核日志打印程序
 * @date 2022-01-28
 *
 * @copyright Copyright (c) 2022 longjin
 *
 */

#pragma once
#include "printk.h"

#define ksuccess(...)                          \
    do                                         \
    {                                          \
        printk("[ ");                          \
        printk_color(GREEN, BLACK, "SUCCESS"); \
        printk(" ] ");                         \
        printk(__VA_ARGS__);                   \
        printk("\n");                          \
    } while (0)

#define kinfo(...)           \
    do                       \
    {                        \
        printk("[ INFO ] "); \
        printk(__VA_ARGS__); \
        printk("\n");        \
    } while (0)

#define kdebug(...)                                        \
    do                                                     \
    {                                                      \
        printk("[ DEBUG ] (%s:%d)\t", __FILE__, __LINE__); \
        printk(__VA_ARGS__);                               \
        printk("\n");                                      \
    } while (0)

#define kwarn(...)                                 \
    do                                             \
    {                                              \
        printk("[ ");                              \
        printk_color(YELLOW, BLACK, "WARN");       \
        printk(" ] "); \
        printk(__VA_ARGS__);                       \
        printk("\n");                              \
    } while (0)

#define kerror(...)                        \
    do                                     \
    {                                      \
        printk("[ ");                      \
        printk_color(RED, BLACK, "ERROR"); \
        printk(" ] ");                     \
        printk(__VA_ARGS__);               \
        printk("\n");                      \
    } while (0)

#define kterminated(...)                        \
    do                                          \
    {                                           \
        printk("[ ");                           \
        printk_color(RED, BLACK, "TERMINATED"); \
        printk(" ] ");                          \
        printk(__VA_ARGS__);                    \
        printk("\n");                           \
    } while (0)

#define kBUG(...)                                   \
    do                                              \
    {                                               \
        printk("[ ");                               \
        printk_color(RED, BLACK, "BUG");            \
        printk(" ] (%s:%d)\t", __FILE__, __LINE__); \
        printk(__VA_ARGS__);                        \
        printk("\n");                               \
    } while (0)
