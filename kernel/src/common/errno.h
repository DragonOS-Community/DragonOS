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
#define EPERM 1   /* 操作不被允许 Operation not permitted. */
#define ENOENT 2  /* 没有指定的文件或目录 No such file or directory. */
#define ESRCH 3   /* 没有这样的进程 No such process. */
#define EINTR 4   /* 被中断的函数 Interrupted function. */
#define EIO 5     /* I/O错误 I/O error. */
#define ENXIO 6   /* 没有这样的设备或地址 No such device or address. */
#define E2BIG 7   /* 参数列表过长，或者在输出buffer中缺少空间 或者参数比系统内建的最大值要大 Argument list too long. */
#define ENOEXEC 8 /* 可执行文件格式错误 Executable file format error */
#define EBADF 9   /* 错误的文件描述符 Bad file descriptor. */
#define ECHILD 10 /* 没有子进程 No child processes. */
/* 操作将被禁止 Operation would block.(may be the same value as [EAGAIN]). */
/* 资源不可用，请重试。 Resource unavailable try again.(may be the same value as [EWOULDBLOCK]) */
#define EAGAIN_OR_EWOULDBLOCK 11
#define ENOMEM 12          /* 没有足够的空间 Not enough space. */
#define EACCES 13          /* 访问被拒绝 Permission denied */
#define EFAULT 14          /* 错误的地址 Bad address */
#define ENOTBLK 15         /* 需要块设备 Block device required */
#define EBUSY 16           /* 设备或资源忙 Device or resource busy. */
#define EEXIST 17          /* 文件已存在 File exists. */
#define EXDEV 18           /* 跨设备连接 Cross-device link. */
#define ENODEV 19          /* 没有指定的设备 No such device. */
#define ENOTDIR 20         /* 不是目录 Not a directory. */
#define EISDIR 21          /* 是一个目录 Is a directory */
#define EINVAL 22          /* 不可用的参数 Invalid argument. */
#define ENFILE 23          /* 系统中打开的文件过多 Too many files open in system. */
#define EMFILE 24          /* 文件描述符的值过大 File descriptor value too large. */
#define ENOTTY 25          /* 不正确的I/O控制操作 Inappropriate I/O control operation. */
#define ETXTBSY 26         /* 文本文件忙 Text file busy. */
#define EFBIG 27           /* 文件太大 File too large. */
#define ENOSPC 28          /* 设备上没有空间 No space left on device. */
#define ESPIPE 29          /* 错误的寻道.当前文件是pipe，不允许seek请求  Invalid seek. */
#define EROFS 30           /* 只读的文件系统 Read-only file system. */
#define EMLINK 31          /* 链接数过多 Too many links. */
#define EPIPE 32           /* 断开的管道 Broken pipe. */
#define EDOM 33            /* 数学参数超出作用域 Mathematics argument out of domain of function. */
#define ERANGE 34          /* 结果过大 Result too large. */
#define EDEADLK 35         /* 资源死锁将要发生 Resource deadlock would occur. */
#define ENAMETOOLONG 36    /* 文件名过长 Filename too long. */
#define ENOLCK 37          /* 没有可用的锁 No locks available. */
#define ENOSYS 38          /* 功能不支持 Function not supported. */
#define ENOTEMPTY 39       /* 目录非空 Directory not empty. */
#define ELOOP 40           /* 符号链接级别过多 Too many levels of symbolic links. */
#define ENOMSG 41          /* 没有期待类型的消息 No message of the desired type. */
#define EIDRM 42           /* 标志符被移除 Identifier removed. */
#define ECHRNG 43          /* 通道号超出范围 Channel number out of range */
#define EL2NSYNC 44        /* 二级不同步 Level 2 not synchronized */
#define EL3HLT 45          /* 三级暂停 Level 3 halted */
#define EL3RST 46          /* 三级重置 Level 3 reset */
#define ELNRNG 47          /* 链接号超出范围 Link number out of range */
#define EUNATCH 48         /* 未连接协议驱动程序 Protocol driver not attached */
#define ENOCSI 49          /* 没有可用的CSI结构 No CSI structure available */
#define EL2HLT 50          /* 二级暂停 Level 2 halted */
#define EBADE 51           /* 无效交换 Invalid exchange */
#define EBADR 52           /* 无效的请求描述符 Invalid request descriptor */
#define EXFULL 53          /* 交换满 Exchange full */
#define ENOANO 54          /* 无阳极 No anode */
#define EBADRQC 55         /* 请求码无效 Invalid request code */
#define EBADSLT 56         /* 无效插槽 Invalid slot */
#define EDEADLOCK 57       /* 资源死锁 Resource deadlock would occur */
#define EBFONT 58          /* 错误的字体文件格式 Bad font file format */
#define ENOSTR 59          /* 不是STREAM Not a STREAM */
#define ENODATA 60         /* 队列头没有可读取的消息 No message is available on the STREAM head read queue. */
#define ETIME 61           /* 流式ioctl()超时 Stream ioctl() timeout */
#define ENOSR 62           /* 没有STREAM资源  No STREAM resources. */
#define ENONET 63          /* 机器不在网络上 Machine is not on the network */
#define ENOPKG 64          /* 未安装软件包 Package not installed */
#define EREMOTE 65         /* 远程对象 Object is remote */
#define ENOLINK 66         /* 保留 Reserved. */
#define EADV 67            /* 外设错误 Advertise error. */
#define ESRMNT 68          /* 安装错误 Srmount error */
#define ECOMM 69           /* 发送时发生通信错误 Communication error on send */
#define EPROTO 70          /* 协议错误 Protocol error. */
#define EMULTIHOP 71       /* 保留使用 Reserved. */
#define EDOTDOT 72         /* RFS特定错误 RFS specific error */
#define EBADMSG 73         /* 错误的消息 Bad message. */
#define EOVERFLOW 74       /* 数值过大，产生溢出 Value too large to be stored in data type. */
#define ENOTUNIQ 75        /* 名称在网络上不是唯一的 Name not unique on network */
#define EBADFD 76          /* 处于不良状态的文件描述符 File descriptor in bad state */
#define EREMCHG 77         /* 远程地址已更改 Remote address changed */
#define ELIBACC 78         /* 无法访问所需的共享库 Can not access a needed shared library */
#define ELIBBAD 79         /* 访问损坏的共享库 Accessing a corrupted shared library */
#define ELIBSCN 80         /* a. out中的.lib部分已损坏 .lib section in a.out corrupted */
#define ELIBMAX 81         /* 尝试链接太多共享库 Attempting to link in too many shared libraries */
#define ELIBEXEC 82        /* 无法直接执行共享库 Cannot exec a shared library directly */
#define EILSEQ 83          /* 不合法的字符序列 Illegal byte sequence. */
#define ERESTART 84        /* 中断的系统调用应该重新启动 Interrupted system call should be restarted */
#define ESTRPIPE 85        /* 流管道错误 Streams pipe error */
#define EUSERS 86          /* 用户太多 Too many users */
#define ENOTSOCK 87        /* 不是一个套接字 Not a socket. */
#define EDESTADDRREQ 88    /* 需要目标地址 Destination address required. */
#define EMSGSIZE 89        /* 消息过大 Message too large. */
#define EPROTOTYPE 90      /* 对于套接字而言，错误的协议 Protocol wrong type for socket. */
#define ENOPROTOOPT 91     /* 协议不可用 Protocol not available. */
#define EPROTONOSUPPORT 92 /* 协议不被支持 Protocol not supported. */
#define ESOCKTNOSUPPORT 93 /* 不支持套接字类型 Socket type not supported */
/* 套接字不支持该操作 Operation not supported on socket (may be the same value as [ENOTSUP]). */
/* 不被支持 Not supported (may be the same value as [EOPNOTSUPP]). */
#define EOPNOTSUPP_OR_ENOTSUP 94
#define EPFNOSUPPORT 95     /* 不支持协议系列 Protocol family not supported */
#define EAFNOSUPPORT 96     /* 地址family不支持 Address family not supported. */
#define EADDRINUSE 97       /* 地址正在被使用 Address in use. */
#define EADDRNOTAVAIL 98    /* 地址不可用 Address  not available. */
#define ENETDOWN 99         /* 网络已关闭 Network is down. */
#define ENETUNREACH 100     /* 网络不可达 Network unreachable. */
#define ENETRESET 101       /* 网络连接已断开 Connection aborted by network. */
#define ECONNABORTED 102    /* 连接已断开 Connection aborted. */
#define ECONNRESET 103      /* 连接被重置 Connection reset. */
#define ENOBUFS 104         /* 缓冲区空间不足 No buffer space available. */
#define EISCONN 105         /* 套接字已连接 Socket is connected. */
#define ENOTCONN 106        /* 套接字未连接 The socket is not connected. */
#define ESHUTDOWN 107       /* 传输端点关闭后无法发送 Cannot send after transport endpoint shutdown */
#define ETOOMANYREFS 108    /* 引用太多：无法拼接 Too many references: cannot splice */
#define ETIMEDOUT 109       /* 连接超时 Connection timed out. */
#define ECONNREFUSED 110    /* 连接被拒绝 Connection refused. */
#define EHOSTDOWN 111       /* 主机已关闭 Host is down */
#define EHOSTUNREACH 112    /* 主机不可达 Host is unreachable. */
#define EALREADY 113        /* 连接已经在处理 Connection already in progress. */
#define EINPROGRESS 114     /* 操作正在处理 Operation in progress. */
#define ESTALE 115          /* 保留 Reserved. */
#define EUCLEAN 116         /* 结构需要清理 Structure needs cleaning */
#define ENOTNAM 117         /* 不是XENIX命名类型文件 Not a XENIX named type file */
#define ENAVAIL 118         /* 没有可用的XENIX信号量 No XENIX semaphores available */
#define EISNAM 119          /* 是命名类型文件 Is a named type file */
#define EREMOTEIO 120       /* 远程I/O错误 Remote I/O error */
#define EDQUOT 121          /* 保留使用 Reserved */
#define ENOMEDIUM 122       /* 没有找到媒介 No medium found */
#define EMEDIUMTYPE 123     /* 介质类型错误 Wrong medium type */
#define ECANCELED 124       /* 操作被取消 Operation canceled. */
#define ENOKEY 125          /* 所需的密钥不可用 Required key not available */
#define EKEYEXPIRED 126     /* 密钥已过期 Key has expired */
#define EKEYREVOKED 127     /* 密钥已被撤销 Key has been revoked */
#define EKEYREJECTED 128    /* 密钥被服务拒绝 Key has been revoked */
#define EOWNERDEAD 129      /* 之前的拥有者挂了 Previous owner died. */
#define ENOTRECOVERABLE 130 /* 状态不可恢复 State not recoverable. */
