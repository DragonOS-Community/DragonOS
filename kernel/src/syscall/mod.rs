use num_traits::{FromPrimitive, ToPrimitive};

#[repr(i32)]
#[derive(Debug, FromPrimitive, ToPrimitive, PartialEq, Eq, Clone)]
#[allow(dead_code)]
pub enum SystemError {
    /// 参数列表过长，或者在输出buffer中缺少空间 或者参数比系统内建的最大值要大 Argument list too long.
    E2BIG = 1,
    /// 访问被拒绝 Permission denied
    EACCES = 2,
    /// 地址正在被使用 Address in use.
    EADDRINUSE = 3,
    /// 地址不可用 Address  not available.
    EADDRNOTAVAIL = 4,
    /// 地址family不支持 Address family not supported.
    EAFNOSUPPORT = 5,
    /// 资源不可用，请重试。 Resource unavailable, try again (may be the same value as [EWOULDBLOCK])
    EAGAIN = 6,
    /// 连接已经在处理 Connection already in progress.
    EALREADY = 7,
    /// 错误的文件描述符 Bad file descriptor.
    EBADF = 8,
    /// 错误的消息 Bad message.
    EBADMSG = 9,
    /// 设备或资源忙 Device or resource busy.
    EBUSY = 10,
    /// 操作被取消 Operation canceled.
    ECANCELED = 11,
    /// 没有子进程 No child processes.
    ECHILD = 12,
    /// 连接已断开 Connection aborted.
    ECONNABORTED = 13,
    /// 连接被拒绝 Connection refused.
    ECONNREFUSED = 14,
    /// 连接被重置 Connection reset.
    ECONNRESET = 15,
    /// 资源死锁将要发生 Resource deadlock would occur.
    EDEADLK = 16,
    /// 需要目标地址 Destination address required.
    EDESTADDRREQ = 17,
    /// 数学参数超出作用域 Mathematics argument out of domain of function.
    EDOM = 18,
    /// 保留使用 Reserved
    EDQUOT = 19,
    /// 文件已存在 File exists.
    EEXIST = 20,
    /// 错误的地址 Bad address
    EFAULT = 21,
    /// 文件太大 File too large.
    EFBIG = 22,
    /// 主机不可达 Host is unreachable.
    EHOSTUNREACH = 23,
    /// 标志符被移除 Identifier removed.
    EIDRM = 24,
    /// 不合法的字符序列 Illegal byte sequence.
    EILSEQ = 25,
    /// 操作正在处理 Operation in progress.
    EINPROGRESS = 26,
    /// 被中断的函数 Interrupted function.
    EINTR = 27,
    /// 不可用的参数 Invalid argument.
    EINVAL = 28,
    /// I/O错误 I/O error.
    EIO = 29,
    /// 套接字已连接 Socket is connected.
    EISCONN = 30,
    /// 是一个目录 Is a directory
    EISDIR = 31,
    /// 符号链接级别过多 Too many levels of symbolic links.
    ELOOP = 32,
    /// 文件描述符的值过大 File descriptor value too large.
    EMFILE = 33,
    /// 链接数过多 Too many links.
    EMLINK = 34,
    /// 消息过大 Message too large.
    EMSGSIZE = 35,
    /// 保留使用 Reserved.
    EMULTIHOP = 36,
    /// 文件名过长 Filename too long.
    ENAMETOOLONG = 37,
    /// 网络已关闭 Network is down.
    ENETDOWN = 38,
    /// 网络连接已断开 Connection aborted by network.
    ENETRESET = 39,
    /// 网络不可达 Network unreachable.
    ENETUNREACH = 40,
    /// 系统中打开的文件过多 Too many files open in system.
    ENFILE = 41,
    /// 缓冲区空间不足 No buffer space available.
    ENOBUFS = 42,
    /// 队列头没有可读取的消息 No message is available on the STREAM head read queue.
    ENODATA = 43,
    /// 没有指定的设备 No such device.
    ENODEV = 44,
    /// 没有指定的文件或目录 No such file or directory.
    ENOENT = 45,
    /// 可执行文件格式错误 Executable file format error
    ENOEXEC = 46,
    /// 没有可用的锁 No locks available.
    ENOLCK = 47,
    /// 保留 Reserved.
    ENOLINK = 48,
    /// 没有足够的空间 Not enough space.
    ENOMEM = 49,
    /// 没有期待类型的消息 No message of the desired type.
    ENOMSG = 50,
    /// 协议不可用 Protocol not available.
    ENOPROTOOPT = 51,
    /// 设备上没有空间 No space left on device.
    ENOSPC = 52,
    /// 没有STREAM资源  No STREAM resources.
    ENOSR = 53,
    /// 不是STREAM Not a STREAM
    ENOSTR = 54,
    /// 功能不支持 Function not supported.
    ENOSYS = 55,
    /// 套接字未连接 The socket is not connected.
    ENOTCONN = 56,
    /// 不是目录 Not a directory.
    ENOTDIR = 57,
    /// 目录非空 Directory not empty.
    ENOTEMPTY = 58,
    /// 状态不可恢复 State not recoverable.
    ENOTRECOVERABLE = 59,
    /// 不是一个套接字 Not a socket.
    ENOTSOCK = 60,
    /// 不被支持 Not supported (may be the same value as [EOPNOTSUPP]).
    ENOTSUP = 61,
    /// 不正确的I/O控制操作 Inappropriate I/O control operation.
    ENOTTY = 62,
    /// 没有这样的设备或地址 No such device or address.
    ENXIO = 63,
    /// 套接字不支持该操作 Operation not supported on socket (may be the same value as [ENOTSUP]).
    EOPNOTSUPP = 64,
    /// 数值过大，产生溢出 Value too large to be stored in data type.
    EOVERFLOW = 65,
    /// 之前的拥有者挂了 Previous owner died.
    EOWNERDEAD = 66,
    /// 操作不被允许 Operation not permitted.
    EPERM = 67,
    /// 断开的管道 Broken pipe.
    EPIPE = 68,
    /// 协议错误 Protocol error.
    EPROTO = 69,
    /// 协议不被支持 Protocol not supported.
    EPROTONOSUPPORT = 70,
    /// 对于套接字而言，错误的协议 Protocol wrong type for socket.
    EPROTOTYPE = 71,
    /// 结果过大 Result too large.
    ERANGE = 72,
    /// 只读的文件系统 Read-only file system.
    EROFS = 73,
    /// 错误的寻道.当前文件是pipe，不允许seek请求  Invalid seek.
    ESPIPE = 74,
    /// 没有这样的进程 No such process.
    ESRCH = 75,
    /// 保留 Reserved.
    ESTALE = 76,
    /// 流式ioctl()超时 Stream ioctl() timeout
    ETIME = 77,
    /// 连接超时 Connection timed out.
    ETIMEDOUT = 78,
    /// 文本文件忙 Text file busy.
    ETXTBSY = 79,
    /// 操作将被禁止 Operation would block (may be the same value as [EAGAIN]).
    EWOULDBLOCK = 80,
    /// 跨设备连接 Cross-device link.
    EXDEV = 81,
}

impl SystemError {
    /// @brief 把posix错误码转换为系统错误枚举类型。
    pub fn from_posix_errno(errno: i32) -> Option<SystemError> {
        // posix 错误码是小于0的
        if errno >= 0 {
            return None;
        }
        return <Self as FromPrimitive>::from_i32(-errno);
    }

    /// @brief 把系统错误枚举类型转换为负数posix错误码。
    pub fn to_posix_errno(&self) -> i32 {
        return -<Self as ToPrimitive>::to_i32(self).unwrap();
    }
}
