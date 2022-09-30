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

int strcmp(const char *FirstPart, const char *SecondPart)
{
    register int __res;
    __asm__ __volatile__("cld	\n\t"
                         "1:	\n\t"
                         "lodsb	\n\t"
                         "scasb	\n\t"
                         "jne	2f	\n\t"
                         "testb	%%al,	%%al	\n\t"
                         "jne	1b	\n\t"
                         "xorl	%%eax,	%%eax	\n\t"
                         "jmp	3f	\n\t"
                         "2:	\n\t"
                         "movl	$1,	%%eax	\n\t"
                         "jl	3f	\n\t"
                         "negl	%%eax	\n\t"
                         "3:	\n\t"
                         : "=a"(__res)
                         : "D"(FirstPart), "S"(SecondPart)
                         :);
    return __res;
}

char *strncpy(char *dst, const char *src, long count)
{
    __asm__ __volatile__("cld	\n\t"
                         "1:	\n\t"
                         "decq	%2	\n\t"
                         "js	2f	\n\t"
                         "lodsb	\n\t"
                         "stosb	\n\t"
                         "testb	%%al,	%%al	\n\t"
                         "jne	1b	\n\t"
                         "rep	\n\t"
                         "stosb	\n\t"
                         "2:	\n\t"
                         :
                         : "S"(src), "D"(dst), "c"(count)
                         : "ax", "memory");
    return dst;
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

