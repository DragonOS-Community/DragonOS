#pragma once
#include <common/sys/types.h>

// ===== 描述long double 的数据比特结构
#if __LDBL_MANT_DIG__ == 53 && __LDBL_MAX_EXP__ == 1024
#elif __LDBL_MANT_DIG__ == 64 && __LDBL_MAX_EXP__ == 16384 && __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
union ldshape
{
    long double f;
    struct
    {
        uint64_t m;
        uint16_t se;
    } i;
};
#elif __LDBL_MANT_DIG__ == 113 && __LDBL_MAX_EXP__ == 16384 && __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
union ldshape
{
    long double f;
    struct
    {
        uint64_t lo;
        uint32_t mid;
        uint16_t top;
        uint16_t se;
    } i;
    struct
    {
        uint64_t lo;
        uint64_t hi;
    } i2;
};
#elif __LDBL_MANT_DIG__ == 113 && __LDBL_MAX_EXP__ == 16384 && __BYTE_ORDER__ == __BIG_ENDIAN
union ldshape
{
    long double f;
    struct
    {
        uint16_t se;
        uint16_t top;
        uint32_t mid;
        uint64_t lo;
    } i;
    struct
    {
        uint64_t hi;
        uint64_t lo;
    } i2;
};
#else
#error Unsupported long double representation
#endif

#define FORCE_EVAL(x)                         \
    do                                        \
    {                                         \
        if (sizeof(x) == sizeof(float))       \
        {                                     \
            volatile float __x;               \
            __x = (x);                        \
            (void)__x;                        \
        }                                     \
        else if (sizeof(x) == sizeof(double)) \
        {                                     \
            volatile double __x;              \
            __x = (x);                        \
            (void)__x;                        \
        }                                     \
        else                                  \
        {                                     \
            volatile long double __x;         \
            __x = (x);                        \
            (void)__x;                        \
        }                                     \
    } while (0)
