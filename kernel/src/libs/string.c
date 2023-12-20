#include <common/string.h>
#include <common/glib.h>

/**
 * @brief 拷贝整个字符串
 *
 * @param dst 目标地址
 * @param src 源地址
 * @return char* 目标字符串
 */
char *strcpy(char *dst, const char *src)
{
    while (*src)
    {
        *(dst++) = *(src++);
    }
    *dst = 0;

    return dst;
}

long strnlen(const char *src, unsigned long maxlen)
{

    if (src == NULL)
        return 0;
    register int __res = 0;
    while (src[__res] != '\0' && __res < maxlen)
    {
        ++__res;
    }
    return __res;
}

/*
        比较字符串 FirstPart and SecondPart
        FirstPart = SecondPart =>  0
        FirstPart > SecondPart =>  1
        FirstPart < SecondPart => -1
*/
int strcmp(const char *l, const char *r)
{
    for (; *l == *r && *l; l++, r++)
        ;
    return *(unsigned char *)l - *(unsigned char *)r;
}

char *__stpncpy(char *restrict d, const char *restrict s, size_t n)
{

    for (; n && (*d = *s); n--, s++, d++)
        ;
tail:
    memset(d, 0, n);
    return d;
}

char *strncpy(char *restrict d, const char *restrict s, size_t n)
{
    __stpncpy(d, s, n);
    return d;
}

long strncpy_from_user(char *dst, const char *src, unsigned long size)
{
    if (!verify_area((uint64_t)src, size))
        return 0;

    strncpy(dst, src, size);
    return size;
}

/**
 * @brief 测量来自用户空间的字符串的长度，会检验地址空间是否属于用户空间
 * @param src
 * @param maxlen
 * @return long
 */
long strnlen_user(const char *src, unsigned long maxlen)
{

    unsigned long size = strlen(src);
    // 地址不合法
    if (!verify_area((uint64_t)src, size))
        return 0;

    return size <= maxlen ? size : maxlen;
}

/**
 * @brief 拼接两个字符串（将src接到dest末尾）
 *
 * @param dest 目标串
 * @param src 源串
 * @return char*
 */
char *strcat(char *dest, const char *src)
{
    strcpy(dest + strlen(dest), src);
    return dest;
}