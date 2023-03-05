#pragma once
#include <common/compiler.h>
#include <DragonOS/stdint.h>
#define MAX_ERRNO 4095

#define IS_ERR_VALUE(x) unlikely((x) >= (uint64_t)-MAX_ERRNO)

/**
 * @brief 判断返回的指针是否为errno
 * 
 * @param ptr 待校验的指针
 * @return long 1 => 是错误码
 *              0 => 不是错误码
 */
static inline long __must_check IS_ERR(const void* ptr)
{
    return IS_ERR_VALUE((uint64_t)ptr);
}

/**
 * @brief 判断返回的指针是否为errno或者为空
 * 
 * @param ptr 待校验的指针
 * @return long 1 => 是错误码或NULL
 *              0 => 不是错误码或NULL
 */
static inline long __must_check IS_ERR_OR_NULL(const void* ptr)
{
    return !ptr || IS_ERR_VALUE((uint64_t)ptr);
}

/**
 * @brief 将错误码转换为指针
 * 
 * @param error 错误码
 * @return void* 转换后的指针
 */
static inline void* __must_check ERR_PTR(long error)
{
    return (void*)(error);
}

static inline long __must_check PTR_ERR(void * ptr)
{
    return (long)ptr;
}