#![no_std]

use num_derive::{FromPrimitive, ToPrimitive};

#[repr(i32)]
#[derive(Debug, FromPrimitive, ToPrimitive, PartialEq, Eq, Clone)]
#[allow(dead_code, non_camel_case_types)]
pub enum SystemError {
    /// 操作不被允许 Operation not permitted.
    EPERM = 1,
    /// 没有指定的文件或目录 No such file or directory.
    ENOENT = 2,
    /// 没有这样的进程 No such process.
    ESRCH = 3,
    /// 被中断的函数 Interrupted function.
    EINTR = 4,
    /// I/O错误 I/O error.
    EIO = 5,
    /// 没有这样的设备或地址 No such device or address.
    ENXIO = 6,
    /// 参数列表过长，或者在输出buffer中缺少空间 或者参数比系统内建的最大值要大 Argument list too long.
    E2BIG = 7,
    /// 可执行文件格式错误 Executable file format error
    ENOEXEC = 8,
    /// 错误的文件描述符 Bad file descriptor.
    EBADF = 9,
    /// 没有子进程 No child processes.
    ECHILD = 10,
    /// 资源不可用，请重试。 Resource unavailable, try again.(may be the same value as [EWOULDBLOCK])
    ///
    /// 操作将被禁止 Operation would block.(may be the same value as [EAGAIN]).
    EAGAIN_OR_EWOULDBLOCK = 11,
    /// 没有足够的空间 Not enough space.
    ENOMEM = 12,
    /// 访问被拒绝 Permission denied
    EACCES = 13,
    /// 错误的地址 Bad address
    EFAULT = 14,
    /// 需要块设备 Block device required
    ENOTBLK = 15,
    /// 设备或资源忙 Device or resource busy.
    EBUSY = 16,
    /// 文件已存在 File exists.
    EEXIST = 17,
    /// 跨设备连接 Cross-device link.
    EXDEV = 18,
    /// 没有指定的设备 No such device.
    ENODEV = 19,
    /// 不是目录 Not a directory.
    ENOTDIR = 20,
    /// 是一个目录 Is a directory
    EISDIR = 21,
    /// 不可用的参数 Invalid argument.
    EINVAL = 22,
    /// 系统中打开的文件过多 Too many files open in system.
    ENFILE = 23,
    /// 文件描述符的值过大 File descriptor value too large.
    EMFILE = 24,
    /// 不正确的I/O控制操作 Inappropriate I/O control operation.
    ENOTTY = 25,
    /// 文本文件忙 Text file busy.
    ETXTBSY = 26,
    /// 文件太大 File too large.
    EFBIG = 27,
    /// 设备上没有空间 No space left on device.
    ENOSPC = 28,
    /// 错误的寻道.当前文件是pipe，不允许seek请求  Invalid seek.
    ESPIPE = 29,
    /// 只读的文件系统 Read-only file system.
    EROFS = 30,
    /// 链接数过多 Too many links.
    EMLINK = 31,
    /// 断开的管道 Broken pipe.
    EPIPE = 32,
    /// 数学参数超出作用域 Mathematics argument out of domain of function.
    EDOM = 33,
    /// 结果过大 Result too large.
    ERANGE = 34,
    /// 资源死锁将要发生 Resource deadlock would occur.
    EDEADLK = 35,
    /// 文件名过长 Filename too long.
    ENAMETOOLONG = 36,
    /// 没有可用的锁 No locks available.
    ENOLCK = 37,
    /// 功能不支持 Function not supported.
    ENOSYS = 38,
    /// 目录非空 Directory not empty.
    ENOTEMPTY = 39,
    /// 符号链接级别过多 Too many levels of symbolic links.
    ELOOP = 40,
    /// 没有期待类型的消息 No message of the desired type.
    ENOMSG = 41,
    /// 标志符被移除 Identifier removed.
    EIDRM = 42,
    /// 通道号超出范围 Channel number out of range
    ECHRNG = 43,
    /// 二级不同步 Level 2 not synchronized
    EL2NSYNC = 44,
    /// 三级暂停 Level 3 halted
    EL3HLT = 45,
    /// 三级重置 Level 3 reset
    EL3RST = 46,
    /// 链接号超出范围 Link number out of range
    ELNRNG = 47,
    /// 未连接协议驱动程序 Protocol driver not attached
    EUNATCH = 48,
    /// 没有可用的CSI结构 No CSI structure available
    ENOCSI = 49,
    /// 二级暂停 Level 2 halted
    EL2HLT = 50,
    /// 无效交换 Invalid exchange
    EBADE = 51,
    /// 无效的请求描述符 Invalid request descriptor
    EBADR = 52,
    /// 交换满 Exchange full
    EXFULL = 53,
    /// 无阳极 No anode
    ENOANO = 54,
    /// 请求码无效 Invalid request code
    EBADRQC = 55,
    /// 无效插槽 Invalid slot
    EBADSLT = 56,
    /// 资源死锁 Resource deadlock would occur
    EDEADLOCK = 57,
    /// 错误的字体文件格式 Bad font file format
    EBFONT = 58,
    /// 不是STREAM Not a STREAM
    ENOSTR = 59,
    /// 队列头没有可读取的消息 No message is available on the STREAM head read queue.
    ENODATA = 60,
    /// 流式ioctl()超时 Stream ioctl() timeout
    ETIME = 61,
    /// 没有STREAM资源  No STREAM resources.
    ENOSR = 62,
    /// 机器不在网络上 Machine is not on the network
    ENONET = 63,
    /// 未安装软件包 Package not installed
    ENOPKG = 64,
    /// 远程对象 Object is remote
    EREMOTE = 65,
    /// 保留 Reserved.
    ENOLINK = 66,
    /// 外设错误 Advertise error.
    EADV = 67,
    /// 安装错误 Srmount error
    ESRMNT = 68,
    /// 发送时发生通信错误 Communication error on send
    ECOMM = 69,
    /// 协议错误 Protocol error.
    EPROTO = 70,
    /// 保留使用 Reserved.
    EMULTIHOP = 71,
    /// RFS特定错误 RFS specific error
    EDOTDOT = 72,
    /// 错误的消息 Bad message.
    EBADMSG = 73,
    /// 数值过大，产生溢出 Value too large to be stored in data type.
    EOVERFLOW = 74,
    /// 名称在网络上不是唯一的 Name not unique on network
    ENOTUNIQ = 75,
    /// 处于不良状态的文件描述符 File descriptor in bad state
    EBADFD = 76,
    /// 远程地址已更改 Remote address changed
    EREMCHG = 77,
    /// 无法访问所需的共享库 Can not access a needed shared library
    ELIBACC = 78,
    /// 访问损坏的共享库 Accessing a corrupted shared library
    ELIBBAD = 79,
    /// a. out中的.lib部分已损坏 .lib section in a.out corrupted
    ELIBSCN = 80,
    /// 尝试链接太多共享库 Attempting to link in too many shared libraries
    ELIBMAX = 81,
    /// 无法直接执行共享库 Cannot exec a shared library directly    
    ELIBEXEC = 82,
    /// 不合法的字符序列 Illegal byte sequence.
    EILSEQ = 83,
    /// 中断的系统调用应该重新启动 Interrupted system call should be restarted
    ERESTART = 84,
    /// 流管道错误 Streams pipe error
    ESTRPIPE = 85,
    /// 用户太多 Too many users
    EUSERS = 86,
    /// 不是一个套接字 Not a socket.
    ENOTSOCK = 87,
    /// 需要目标地址 Destination address required.
    EDESTADDRREQ = 88,
    /// 消息过大 Message too large.
    EMSGSIZE = 89,
    /// 对于套接字而言，错误的协议 Protocol wrong type for socket.
    EPROTOTYPE = 90,
    /// 协议不可用 Protocol not available.
    ENOPROTOOPT = 91,
    /// 协议不被支持 Protocol not supported.
    EPROTONOSUPPORT = 92,
    /// 不支持套接字类型 Socket type not supported
    ESOCKTNOSUPPORT = 93,
    /// 套接字不支持该操作 Operation not supported on socket (may be the same value as [ENOTSUP]).
    ///
    /// 不被支持 Not supported (may be the same value as [EOPNOTSUPP]).
    EOPNOTSUPP_OR_ENOTSUP = 94,
    /// 不支持协议系列 Protocol family not supported
    EPFNOSUPPORT = 95,
    /// 地址family不支持 Address family not supported.
    EAFNOSUPPORT = 96,
    /// 地址正在被使用 Address in use.
    EADDRINUSE = 97,
    /// 地址不可用 Address  not available.
    EADDRNOTAVAIL = 98,
    /// 网络已关闭 Network is down.
    ENETDOWN = 99,
    /// 网络不可达 Network unreachable.
    ENETUNREACH = 100,
    /// 网络连接已断开 Connection aborted by network.
    ENETRESET = 101,
    /// 连接已断开 Connection aborted.
    ECONNABORTED = 102,
    /// 连接被重置 Connection reset.
    ECONNRESET = 103,
    /// 缓冲区空间不足 No buffer space available.
    ENOBUFS = 104,
    /// 套接字已连接 Socket is connected.
    EISCONN = 105,
    /// 套接字未连接 The socket is not connected.
    ENOTCONN = 106,
    /// 传输端点关闭后无法发送 Cannot send after transport endpoint shutdown
    ESHUTDOWN = 107,
    /// 引用太多：无法拼接 Too many references: cannot splice
    ETOOMANYREFS = 108,
    /// 连接超时 Connection timed out.
    ETIMEDOUT = 109,
    /// 连接被拒绝 Connection refused.
    ECONNREFUSED = 110,
    /// 主机已关闭 Host is down
    EHOSTDOWN = 111,
    /// 主机不可达 Host is unreachable.
    EHOSTUNREACH = 112,
    /// 连接已经在处理 Connection already in progress.
    EALREADY = 113,
    /// 操作正在处理 Operation in progress.
    EINPROGRESS = 114,
    /// 保留 Reserved.
    ESTALE = 115,
    /// 结构需要清理 Structure needs cleaning
    EUCLEAN = 116,
    /// 不是XENIX命名类型文件 Not a XENIX named type file
    ENOTNAM = 117,
    /// 没有可用的XENIX信号量 No XENIX semaphores available
    ENAVAIL = 118,
    /// 是命名类型文件 Is a named type file    
    EISNAM = 119,
    /// 远程I/O错误 Remote I/O error
    EREMOTEIO = 120,
    /// 保留使用 Reserved
    EDQUOT = 121,
    /// 没有找到媒介 No medium found
    ENOMEDIUM = 122,
    /// 介质类型错误 Wrong medium type
    EMEDIUMTYPE = 123,
    /// 操作被取消 Operation canceled.
    ECANCELED = 124,
    /// 所需的密钥不可用 Required key not available
    ENOKEY = 125,
    /// 密钥已过期 Key has expired
    EKEYEXPIRED = 126,
    /// 密钥已被撤销 Key has been revoked
    EKEYREVOKED = 127,
    /// 密钥被服务拒绝 Key has been revoked
    EKEYREJECTED = 128,
    /// 之前的拥有者挂了 Previous owner died.
    EOWNERDEAD = 129,
    /// 状态不可恢复 State not recoverable.
    ENOTRECOVERABLE = 130,
    // VMX on 虚拟化开启指令出错
    EVMXONFailed = 131,
    // VMX off 虚拟化关闭指令出错
    EVMXOFFFailed = 132,
    // VMX VMWRITE 写入虚拟化VMCS内存出错
    EVMWRITEFailed = 133,
    EVMREADFailed = 134,
    EVMPRTLDFailed = 135,
    EVMLAUNCHFailed = 136,
    KVM_HVA_ERR_BAD = 137,

    // === 以下错误码不应该被用户态程序使用 ===
    ERESTARTSYS = 512,
}

impl SystemError {
    /// @brief 把posix错误码转换为系统错误枚举类型。
    pub fn from_posix_errno(errno: i32) -> Option<SystemError> {
        // posix 错误码是小于0的
        if errno >= 0 {
            return None;
        }
        return <Self as num_traits::FromPrimitive>::from_i32(-errno);
    }

    /// @brief 把系统错误枚举类型转换为负数posix错误码。
    pub fn to_posix_errno(&self) -> i32 {
        return -<Self as num_traits::ToPrimitive>::to_i32(self).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        assert_eq!(SystemError::EPERM.to_posix_errno(), -1);
    }
}
