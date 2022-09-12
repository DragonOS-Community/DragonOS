#pragma once

#include <stdint.h>

// 系统调用号
#define SYS_NOT_EXISTS 0
#define SYS_PUT_STRING 1
#define SYS_OPEN 2
#define SYS_CLOSE 3
#define SYS_READ 4
#define SYS_WRITE 5
#define SYS_LSEEK 6
#define SYS_FORK 7
#define SYS_VFORK 8
#define SYS_BRK 9
#define SYS_SBRK 10

#define SYS_REBOOT 11   // 重启
#define SYS_CHDIR 12    // 切换工作目录
#define SYS_GET_DENTS 13 // 获取目录中的数据
#define SYS_EXECVE 14 // 执行新的应用程序
#define SYS_WAIT4 15 // 等待进程退出
#define SYS_EXIT 16 // 进程退出
#define SYS_MKDIR 17 // 创建文件夹
#define SYS_NANOSLEEP 18 // 纳秒级休眠
#define SYS_CLOCK 19 // 获取当前cpu时间
#define SYS_PIPE 20

#define SYS_MSTAT 21    // 获取系统的内存状态信息
#define SYS_RMDIR 22    // 删除文件夹

/**
 * @brief 用户态系统调用函数
 *
 * @param syscall_id
 * @param arg0
 * @param arg1
 * @param arg2
 * @param arg3
 * @param arg4
 * @param arg5
 * @param arg6
 * @param arg7
 * @return long
 */
long syscall_invoke(uint64_t syscall_id, uint64_t arg0, uint64_t arg1, uint64_t arg2, uint64_t arg3, uint64_t arg4, uint64_t arg5, uint64_t arg6, uint64_t arg7);