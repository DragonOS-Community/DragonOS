#pragma once

#include <libc/src/include/types.h>

void *memset(void *dst, unsigned char C, uint64_t size);
/**
 * @brief 获取字符串的大小
 *
 * @param s 字符串
 * @return size_t 大小
 */
size_t strlen(const char *s);

/*
        比较字符串 FirstPart and SecondPart
        FirstPart = SecondPart =>  0
        FirstPart > SecondPart =>  1
        FirstPart < SecondPart => -1
*/

int strcmp(const char *FirstPart, const char *SecondPart);

/**
 * @brief 拷贝指定字节数的字符串
 *
 * @param dst 目标地址
 * @param src 源字符串
 * @param Count 字节数
 * @return char*
 */
char *strncpy(char *dst, const char *src, size_t Count);

/**
 * @brief 拷贝整个字符串
 * 
 * @param dst 目标地址
 * @param src 源地址
 * @return char* 目标字符串
 */
char* strcpy(char* dst, const char* src);

/**
 * @brief 拼接两个字符串（将src接到dest末尾）
 *
 * @param dest 目标串
 * @param src 源串
 * @return char*
 */
char *strcat(char *dest, const char *src);

/**
 * @brief 内存拷贝函数
 *
 * @param dst 目标数组
 * @param src 源数组
 * @param Num 字节数
 * @return void*
 */
static void *memcpy(void *dst, const void *src, long Num)
{
    int d0 = 0, d1 = 0, d2 = 0;
    __asm__ __volatile__("cld	\n\t"
                         "rep	\n\t"
                         "movsq	\n\t"
                         "testb	$4,%b4	\n\t"
                         "je	1f	\n\t"
                         "movsl	\n\t"
                         "1:\ttestb	$2,%b4	\n\t"
                         "je	2f	\n\t"
                         "movsw	\n\t"
                         "2:\ttestb	$1,%b4	\n\t"
                         "je	3f	\n\t"
                         "movsb	\n\t"
                         "3:	\n\t"
                         : "=&c"(d0), "=&D"(d1), "=&S"(d2)
                         : "0"(Num / 8), "q"(Num), "1"(dst), "2"(src)
                         : "memory");
    return dst;
}