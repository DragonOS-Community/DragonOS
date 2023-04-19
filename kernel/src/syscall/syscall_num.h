#pragma once

/**
 * 系统调用说明
 * 1    printf
 *
 *
 * 255  AHCI end_request
 *
 */

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

#define SYS_REBOOT 11    // 重启
#define SYS_CHDIR 12     // 切换工作目录
#define SYS_GET_DENTS 13 // 获取目录中的数据
#define SYS_EXECVE 14    // 执行新的应用程序
#define SYS_WAIT4 15     // 等待进程退出
#define SYS_EXIT 16      // 进程退出
#define SYS_MKDIR 17     // 创建文件夹
#define SYS_NANOSLEEP 18 // 纳秒级休眠
#define SYS_CLOCK 19     // 获取当前cpu时间
#define SYS_PIPE 20      // 创建管道

#define SYS_MSTAT 21        // 获取系统的内存状态信息
#define SYS_UNLINK_AT 22    // 删除文件夹/删除文件链接
#define SYS_KILL 23         // kill一个进程(向这个进程发出信号)
#define SYS_SIGACTION 24    // 设置进程的信号处理动作
#define SYS_RT_SIGRETURN 25 // 从信号处理函数返回
#define SYS_GETPID 26       // 获取当前进程的pid（进程标识符）
#define SYS_SCHED 27        // 让系统立即运行调度器（该系统调用不能由运行在Ring3的程序发起）
#define SYS_DUP 28
#define SYS_DUP2 29
#define SYS_SOCKET 30 // 创建一个socket

#define SYS_SETSOCKOPT 31 // 设置socket的选项
#define SYS_GETSOCKOPT 32 // 获取socket的选项
#define SYS_CONNECT 33    // 连接到一个socket
#define SYS_BIND 34       // 绑定一个socket
#define SYS_SENDTO 35     // 向一个socket发送数据
#define SYS_RECVFROM 36   // 从一个socket接收数据
#define SYS_RECVMSG 37    // 从一个socket接收消息
#define SYS_LISTEN 38     // 监听一个socket
#define SYS_SHUTDOWN 39   // 关闭socket
#define SYS_ACCEPT 40     // 接受一个socket连接
#define SYS_GETSOCKNAME 41 // 获取socket的名字
#define SYS_GETPEERNAME 42 // 获取socket的对端名字

#define SYS_AHCI_END_REQ 255 // AHCI DMA请求结束end_request的系统调用