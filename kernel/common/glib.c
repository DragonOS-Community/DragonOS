#include "glib.h"

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