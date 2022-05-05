#pragma once
#include <stdint.h>
#include <libc/sys/types.h>

// 字体颜色的宏定义
#define COLOR_WHITE 0x00ffffff  //白
#define COLOR_BLACK 0x00000000  //黑
#define COLOR_RED 0x00ff0000    //红
#define COLOR_ORANGE 0x00ff8000 //橙
#define COLOR_YELLOW 0x00ffff00 //黄
#define COLOR_GREEN 0x0000ff00  //绿
#define COLOR_BLUE 0x000000ff   //蓝
#define COLOR_INDIGO 0x0000ffff //靛
#define COLOR_PURPLE 0x008000ff //紫


/**
 * @brief 往屏幕上输出字符串
 * 
 * @param str 字符串指针
 * @param front_color 前景色
 * @param bg_color 背景色
 * @return int64_t 
 */
int64_t put_string(char* str, uint64_t front_color, uint64_t bg_color);


/**
 * @brief 关闭文件接口
 *
 * @param fd 文件描述符
 * @return int
 */
int close(int fd);

/**
 * @brief 从文件读取数据的接口
 *
 * @param fd 文件描述符
 * @param buf 缓冲区
 * @param count 待读取数据的字节数
 * @return ssize_t 成功读取的字节数
 */
ssize_t read(int fd, void *buf, size_t count);

/**
 * @brief 向文件写入数据的接口
 *
 * @param fd 文件描述符
 * @param buf 缓冲区
 * @param count 待写入数据的字节数
 * @return ssize_t 成功写入的字节数
 */
ssize_t write(int fd, void const *buf, size_t count);

/**
 * @brief 调整文件的访问位置
 *
 * @param fd 文件描述符号
 * @param offset 偏移量
 * @param whence 调整模式
 * @return uint64_t 调整结束后的文件访问位置
 */
off_t lseek(int fd, off_t offset, int whence);

/**
 * @brief fork当前进程
 * 
 * @return pid_t 
 */
pid_t fork(void);

/**
 * @brief fork当前进程，但是与父进程共享VM、flags、fd
 * 
 * @return pid_t 
 */
pid_t vfork(void);

