# string.h

## 简介:

    字符串操作

## 函数列表:

    ``size_t strlen(const char *s)`` : 获取字符串的长度

    ``int strcmp(const char *a,const char *b)`` : 比较字符串的字典序

    ``char *strncpy(char *dst, const char *src, size_t count)`` 

        拷贝制定字节数的字符串
        dst: 目标地址
        src： 原字符串
        count: 字节数

    ``char *strcpy(char *dst,const char *src)`` : 复制整个字符串

    ``char *strcat(char *dest,const char* src)`` : 拼接两个字符串

    ``char *strtok(char *str, const char *delim)`` : 分割字符串

    ``char *strtok_r(char *str, const char *delim, char **saveptr)`` : 分割字符串

**以下函数没有经过检验，不确保正常工作**

    ``size_t strspn(const char *str1, const char *str2)`` : 检索字符串 str1 中第一个不在字符串 str2 中出现的字符下标

    ``size_t strcspn(const char *str1, const char *str2)`` : 检索字符串 str1 开头连续有几个字符都不含字符串 str2 中的字符

    ``char *strpbrk(const char *str1, const char *str2)`` : 检索字符串 str1 中第一个匹配字符串 str2 中字符的字符

    ``char *strchr(const char *str, int c)`` : 在字符串中查找第一次出现的字符

    ``char *strrchr(const char *str, int c)`` : 在字符串中查找最后一次出现的字符