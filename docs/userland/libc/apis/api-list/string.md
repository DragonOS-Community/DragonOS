# string.h

## 简介:

    字符串操作

## 函数列表:

    ``size_t strlen(const char *s)`` : 返回字符串长度
    
    ``int strcmp(const char *a,const char *b)`` 比较字符串的字典序

    ``char* strncpy(char *dst,const char *src,size_t count)`` 

        拷贝制定字节数的字符串

        dst: 目标地址

        src： 原字符串

        count: 字节数
    
    ``char* strcpy(char *dst,const char *src)`` : 复制整个字符串

    ``char* strcat(char *dest,const char* src)`` : 拼接两个字符串