#pragma once
#include <libc/sys/types.h>

/**
 * @brief 获取一块堆内存
 * 
 * @param size 内存大小
 * @return void* 内存空间的指针
 */
void *malloc(ssize_t size);

/**
 * @brief 释放一块堆内存
 * 
 * @param ptr 堆内存的指针
 */
void free(void* ptr);

/**
 * @brief 返回int的绝对值
 * 
 * @param i 
 * @return int 
 */
int abs(int i);
long labs(long i);
long long llabs(long long i);

/**
 * @brief 字符串转int
 * 
 * @param str 
 * @return int 
 */
int atoi(const char * str);
// 最大的随机数值
#define	RAND_MAX 2147483647
/**
 * @brief 生成随机数
 * 
 * @return int 随机数
 */
int rand(void);
/**
 * @brief 设置随机数种子
 * 
 * @param seed 种子
 */
void srand(unsigned seed);
/**
 * @brief 退出进程
 * 
 * @param status 
 */
void exit(int status);