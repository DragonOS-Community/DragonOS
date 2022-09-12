#pragma once
#include <stdint.h>
#include <libc/sys/types.h>

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

/**
 * @brief 将堆内存调整为end_brk
 *
 * @param end_brk 新的堆区域的结束地址
 * end_brk=-1  ===> 返回堆区域的起始地址
 * end_brk=-2  ===> 返回堆区域的结束地址
 * @return uint64_t 错误码
 *
 */
uint64_t brk(uint64_t end_brk);

/**
 * @brief 将堆内存空间加上offset（注意，该系统调用只应在普通进程中调用，而不能是内核线程）
 *
 * @param increment offset偏移量
 * @return uint64_t the previous program break
 */
void *sbrk(int64_t increment);

/**
 * @brief 切换当前工作目录
 *
 * @param dest_path 目标目录
 * @return int64_t 成功：0,失败：负值（错误码）
 */
int64_t chdir(char *dest_path);

/**
 * @brief 执行新的程序
 * 
 * @param path 文件路径
 * @param argv 参数列表
 * @return int 
 */
int execv(const char* path, char * const argv[]);

/**
 * @brief 睡眠指定时间
 * 
 * @param usec 微秒
 * @return int 
 */
extern int usleep(useconds_t usec);

/**
 * @brief 删除文件夹
 * 
 * @param path 绝对路径
 * @return int 错误码
 */
int rmdir(const char* path);