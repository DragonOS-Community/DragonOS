#pragma once
#include "glib.h"
/**
 * @brief 拷贝整个字符串
 *
 * @param dst 目标地址
 * @param src 源地址
 * @return char* 目标字符串
 */
char *strcpy(char *dst, const char *src);

//计算字符串的长度（经过测试，该版本比采用repne/scasb汇编的运行速度快16.8%左右）
static inline int strlen(const char *s)
{
    if (s == NULL)
        return 0;
    register int __res = 0;
    while (s[__res] != '\0')
    {
        ++__res;
    }
    return __res;
}

/**
 * @brief 测量字符串的长度
 *
 * @param src 字符串
 * @param maxlen 最大长度
 * @return long
 */
long strnlen(const char *src, unsigned long maxlen);

/*
        比较字符串 FirstPart and SecondPart
        FirstPart = SecondPart =>  0
        FirstPart > SecondPart =>  1
        FirstPart < SecondPart => -1
*/

int strcmp(const char *FirstPart, const char *SecondPart);

char *strncpy(char *dst, const char *src, long count);

long strncpy_from_user(char *dst, const char *src, unsigned long size);

/**
 * @brief 测量来自用户空间的字符串的长度，会检验地址空间是否属于用户空间
 * @param src
 * @param maxlen
 * @return long
 */
long strnlen_user(const char *src, unsigned long maxlen);

/**
 * @brief 逐字节比较指定内存区域的值，并返回s1、s2的第一个不相等的字节i处的差值（s1[i]-s2[i])。
 * 若两块内存区域的内容相同，则返回0
 *
 * @param s1 内存区域1
 * @param s2 内存区域2
 * @param len 要比较的内存区域长度
 * @return int s1、s2的第一个不相等的字节i处的差值（s1[i]-s2[i])。若两块内存区域的内容相同，则返回0
 */
static inline int memcmp(const void *s1, const void *s2, size_t len)
{
    int diff;

    asm("cld \n\t"  // 复位DF，确保s1、s2指针是自增的
        "repz; cmpsb\n\t" CC_SET(nz)
        : CC_OUT(nz)(diff), "+D"(s1), "+S"(s2)
        : "c"(len)
        : "memory");

    if (diff)
        diff = *(const unsigned char *)(s1 - 1) - *(const unsigned char *)(s2 - 1);

    return diff;
}