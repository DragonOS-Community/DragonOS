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

char *strncpy(char *restrict d, const char *restrict s, size_t n);

long strncpy_from_user(char *dst, const char *src, unsigned long size);

/**
 * @brief 测量来自用户空间的字符串的长度，会检验地址空间是否属于用户空间
 * @param src
 * @param maxlen
 * @return long
 */
long strnlen_user(const char *src, unsigned long maxlen);

/**
 * @brief 拼接两个字符串（将src接到dest末尾）
 *
 * @param dest 目标串
 * @param src 源串
 * @return char*
 */
char *strcat(char *dest, const char *src);