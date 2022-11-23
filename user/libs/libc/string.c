#include "string.h"

size_t strlen(const char *s)
{
    register int __res = 0;
    while (s[__res] != '\0')
    {
        ++__res;
    }
    return __res;
}

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

void *memset(void *dst, unsigned char C, uint64_t size)
{

    int d0, d1;
    unsigned long tmp = C * 0x0101010101010101UL;
    __asm__ __volatile__("cld	\n\t"
                         "rep	\n\t"
                         "stosq	\n\t"
                         "testb	$4, %b3	\n\t"
                         "je	1f	\n\t"
                         "stosl	\n\t"
                         "1:\ttestb	$2, %b3	\n\t"
                         "je	2f\n\t"
                         "stosw	\n\t"
                         "2:\ttestb	$1, %b3	\n\t"
                         "je	3f	\n\t"
                         "stosb	\n\t"
                         "3:	\n\t"
                         : "=&c"(d0), "=&D"(d1)
                         : "a"(tmp), "q"(size), "0"(size / 8), "1"(dst)
                         : "memory");
    return dst;
}

/**
 * @brief 拷贝指定字节数的字符串
 *
 * @param dst 目标地址
 * @param src 源字符串
 * @param Count 字节数
 * @return char*
 */
char *strncpy(char *dst, const char *src, size_t Count)
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
                         : "S"(src), "D"(dst), "c"(Count)
                         : "ax", "memory");
    return dst;
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