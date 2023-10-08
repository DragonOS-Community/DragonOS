#pragma once

#include <sys/types.h>

#if defined(__cplusplus) 
extern  "C"  { 
#endif

void *memset(void *dst, unsigned char C, uint64_t size);

/**
 * @brief 获取字符串的长度
 *
 * @param s 字符串
 * @return size_t 长度
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
char *strncpy(char *dst, const char *src, size_t count);

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

/**
 * @brief 分割字符串
 *
 * @param str 要被分解成一组小字符串的字符串
 * @param delim 包含分隔符的字符串
 * @return 分割结果
 */
char *strtok(char *str, const char *delim);

/**
 * @brief 分割字符串
 *
 * @param str 要被分解成一组小字符串的字符串
 * @param delim 包含分隔符的字符串
 * @param saveptr 用于存储当前操作的字符串
 * @return 分割结果
 */
char *strtok_r(char *str, const char *delim, char **saveptr);

//! 以下函数没有经过检验，不确保正常工作

/**
 * @brief 检索字符串 str1 中第一个不在字符串 str2 中出现的字符下标
 *
 * @param str1 被检索的字符串
 * @param str2 进行匹配的字符列表
 * @return str1 中第一个不在字符串 str2 中出现的字符下标
 */
size_t strspn(const char *str1, const char *str2);

/**
 * @brief 检索字符串 str1 开头连续有几个字符都不含字符串 str2 中的字符
 *
 * @param str1 被检索的字符串
 * @param str2 进行匹配的字符列表
 * @return str1 开头连续都不含字符串 str2 中字符的字符数
 */
size_t strcspn(const char *str1, const char *str2);

/**
 * @brief 检索字符串 str1 中第一个匹配字符串 str2 中字符的字符
 *
 * @param str1 被检索的字符串
 * @param str2 进行匹配的字符列表
 * @return str1 中第一个匹配字符串 str2 中字符的指针，如果未找到字符则返回 NULL
 */
char *strpbrk(const char *str1, const char *str2);

/**
 * @brief 在字符串中查找第一次出现的字符
 *
 * @param str 被查找的字符串
 * @param c 要查找的字符
 * @return 指向找到的字符的指针，如果未找到该字符则返回 NULL
 */
char *strchr(const char *str, int c);

/**
 * @brief 在字符串中查找最后一次出现的字符
 *
 * @param str 被查找的字符串
 * @param c 要查找的字符
 * @return 指向找到的字符的指针，如果未找到该字符则返回 NULL
 */
char *strrchr(const char *str, int c);

#if defined(__cplusplus) 
}  /* extern "C" */ 
#endif
