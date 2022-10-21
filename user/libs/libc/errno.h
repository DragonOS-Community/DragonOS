/**
 * @file errno.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief
 * @version 0.1
 * @date 2022-04-22
 *
 * @copyright Copyright (c) 2022
 *
 */
#pragma once

#define E2BIG 1            /* 参数列表过长，或者在输出buffer中缺少空间 或者参数比系统内建的最大值要大 Argument list too long. */
#define EACCES 2           /* 访问被拒绝 Permission denied */
#define EADDRINUSE 3       /* 地址正在被使用 Address in use.*/
#define EADDRNOTAVAIL 4    /* 地址不可用 Address  not available.*/
#define EAFNOSUPPORT 5     /* 地址family不支持 Address family not supported. */
#define EAGAIN 6           /* 资源不可用，请重试。 Resource unavailable, try again (may be the same value as [EWOULDBLOCK]).*/
#define EALREADY 7         /* 连接已经在处理 Connection already in progress. */
#define EBADF 8            /* 错误的文件描述符 Bad file descriptor. */
#define EBADMSG 9          /* 错误的消息 Bad message. */

#define EBUSY 10           /* 设备或资源忙 Device or resource busy. */
#define ECANCELED 11       /* 操作被取消 Operation canceled. */
#define ECHILD 12          /* 没有子进程 No child processes. */
#define ECONNABORTED 13    /* 连接已断开 Connection aborted. */
#define ECONNREFUSED 14    /* 连接被拒绝 Connection refused. */
#define ECONNRESET 15      /* 连接被重置 Connection reset. */
#define EDEADLK 16         /* 资源死锁将要发生 Resource deadlock would occur. */
#define EDESTADDRREQ 17    /* 需要目标地址 Destination address required.*/
#define EDOM 18            /* 数学参数超出作用域 Mathematics argument out of domain of function. */
#define EDQUOT 19          /* 保留使用 Reserved */

#define EEXIST 20          /* 文件已存在 File exists. */
#define EFAULT 21          /* 错误的地址 Bad address */
#define EFBIG 22           /* 文件太大 File too large. */
#define EHOSTUNREACH 23    /* 主机不可达 Host is unreachable.*/
#define EIDRM 24           /* 标志符被移除 Identifier removed. */
#define EILSEQ 25          /* 不合法的字符序列 Illegal byte sequence. */
#define EINPROGRESS 26     /* 操作正在处理 Operation in progress. */
#define EINTR 27           /* 被中断的函数 Interrupted function. */
#define EINVAL 28          /* 不可用的参数 Invalid argument. */
#define EIO 29             /* I/O错误 I/O error. */

#define EISCONN 30         /* 套接字已连接 Socket is connected. */
#define EISDIR 31          /* 是一个目录 Is a directory */
#define ELOOP 32           /* 符号链接级别过多 Too many levels of symbolic links. */
#define EMFILE 33          /* 文件描述符的值过大 File descriptor value too large. */
#define EMLINK 34          /* 链接数过多 Too many links. */
#define EMSGSIZE 35        /* 消息过大 Message too large. */
#define EMULTIHOP 36       /* 保留使用 Reserved. */
#define ENAMETOOLONG 37    /* 文件名过长 Filename too long. */
#define ENETDOWN 38        /* 网络已关闭 Network is down. */
#define ENETRESET 39       /* 网络连接已断开 Connection aborted by network. */

#define ENETUNREACH 40     /* 网络不可达 Network unreachable. */
#define ENFILE 41          /* 系统中打开的文件过多 Too many files open in system.*/
#define ENOBUFS 42         /* 缓冲区空间不足 No buffer space available. */
#define ENODATA 43         /* 队列头没有可读取的消息 No message is available on the STREAM head read queue. */
#define ENODEV 44          /* 没有指定的设备 No such device. */
#define ENOENT 45          /* 没有指定的文件或目录 No such file or directory. */
#define ENOEXEC 46         /* 可执行文件格式错误 Executable file format error. */
#define ENOLCK 47          /* 没有可用的锁 No locks available. */
#define ENOLINK 48         /* 保留 Reserved. */
#define ENOMEM 49          /* 没有足够的空间 Not enough space. */

#define ENOMSG 50          /* 没有期待类型的消息 No message of the desired type. */
#define ENOPROTOOPT 51     /* 协议不可用 Protocol not available. */
#define ENOSPC 52          /* 设备上没有空间 No space left on device. */
#define ENOSR 53           /* 没有STREAM资源  No STREAM resources.*/
#define ENOSTR 54          /* 不是STREAM Not a STREAM */
#define ENOSYS 55          /* 功能不支持 Function not supported. */
#define ENOTCONN 56        /* 套接字未连接 The socket is not connected. */
#define ENOTDIR 57         /* 不是目录 Not a directory. */
#define ENOTEMPTY 58       /* 目录非空 Directory not empty. */
#define ENOTRECOVERABLE 59 /* 状态不可覆盖 State not recoverable. */

#define ENOTSOCK 60        /* 不是一个套接字 Not a socket.*/
#define ENOTSUP 61         /* 不被支持 Not supported (may be the same value as [EOPNOTSUPP]). */
#define ENOTTY 62          /* 不正确的I/O控制操作 Inappropriate I/O control operation. */
#define ENXIO 63           /* 没有这样的设备或地址 No such device or address. */
#define EOPNOTSUPP 64      /* 套接字不支持该操作 Operation not supported on socket (may be the same value as [ENOTSUP]). */
#define EOVERFLOW 65       /* 数值过大，产生溢出 Value too large to be stored in data type. */
#define EOWNERDEAD 66      /* 之前的拥有者挂了 Previous owner died. */
#define EPERM 67           /* 操作不被允许 Operation not permitted. */
#define EPIPE 68           /* 断开的管道 Broken pipe. */
#define EPROTO 69          /* 协议错误 Protocol error. */

#define EPROTONOSUPPORT 70 /* 协议不被支持 Protocol not supported. */
#define EPROTOTYPE 71      /* 对于套接字而言，错误的协议 Protocol wrong type for socket. */
#define ERANGE 72          /* 结果过大 Result too large. */
#define EROFS 73           /* 只读的文件系统 Read-only file system. */
#define ESPIPE 74          /* 错误的寻道 Invalid seek. */
#define ESRCH 75           /* 没有这样的进程 No such process. */
#define ESTALE 76          /* 保留 Reserved. */
#define ETIME 77           /* 流式ioctl()超时 Stream ioctl() timeout */
#define ETIMEDOUT 78       /* 连接超时 Connection timed out.*/
#define ETXTBSY 79         /* 文本文件忙 Text file busy. */

#define EWOULDBLOCK 80     /* 操作将被禁止 Operation would block (may be the same value as [EAGAIN]). */
#define EXDEV 81           /* 跨设备连接 Cross-device link. */

extern int errno;