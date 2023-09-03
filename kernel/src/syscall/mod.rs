use core::{
    ffi::{c_char, c_int, c_void, CStr},
    sync::atomic::{AtomicBool, Ordering},
};

use num_traits::{FromPrimitive, ToPrimitive};

use crate::{
    arch::{cpu::cpu_reset, interrupt::TrapFrame, MMArch},
    filesystem::vfs::{
        fcntl::FcntlCommand,
        file::FileMode,
        io::SeekFrom,
        syscall::{PosixKstat, SEEK_CUR, SEEK_END, SEEK_MAX, SEEK_SET},
        MAX_PATHLEN,
    },
    include::bindings::bindings::{pid_t, PAGE_2M_SIZE, PAGE_4K_SIZE},
    kinfo,
    libs::align::page_align_up,
    mm::{verify_area, MemoryManagementArch, VirtAddr},
    net::syscall::SockAddr,
    process::Pid,
    time::{
        syscall::{PosixTimeZone, PosixTimeval},
        TimeSpec,
    },
};

use self::user_access::UserBufferWriter;

pub mod user_access;

#[repr(i32)]
#[derive(Debug, FromPrimitive, ToPrimitive, PartialEq, Eq, Clone)]
#[allow(dead_code, non_camel_case_types)]
pub enum SystemError {
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

// 定义系统调用号
pub const SYS_PUT_STRING: usize = 1;
pub const SYS_OPEN: usize = 2;
pub const SYS_CLOSE: usize = 3;
pub const SYS_READ: usize = 4;
pub const SYS_WRITE: usize = 5;
pub const SYS_LSEEK: usize = 6;
pub const SYS_FORK: usize = 7;
pub const SYS_VFORK: usize = 8;
pub const SYS_BRK: usize = 9;
pub const SYS_SBRK: usize = 10;

pub const SYS_REBOOT: usize = 11;
pub const SYS_CHDIR: usize = 12;
pub const SYS_GET_DENTS: usize = 13;
pub const SYS_EXECVE: usize = 14;
pub const SYS_WAIT4: usize = 15;
pub const SYS_EXIT: usize = 16;
pub const SYS_MKDIR: usize = 17;
pub const SYS_NANOSLEEP: usize = 18;
/// todo: 该系统调用与Linux不一致，将来需要删除该系统调用！！！ 删的时候记得改C版本的libc
pub const SYS_CLOCK: usize = 19;
pub const SYS_PIPE: usize = 20;
/// 系统调用21曾经是SYS_MSTAT，但是现在已经废弃
pub const __NOT_USED: usize = 21;
pub const SYS_UNLINK_AT: usize = 22;
pub const SYS_KILL: usize = 23;
pub const SYS_SIGACTION: usize = 24;
pub const SYS_RT_SIGRETURN: usize = 25;
pub const SYS_GETPID: usize = 26;
pub const SYS_SCHED: usize = 27;
pub const SYS_DUP: usize = 28;
pub const SYS_DUP2: usize = 29;
pub const SYS_SOCKET: usize = 30;

pub const SYS_SETSOCKOPT: usize = 31;
pub const SYS_GETSOCKOPT: usize = 32;
pub const SYS_CONNECT: usize = 33;
pub const SYS_BIND: usize = 34;
pub const SYS_SENDTO: usize = 35;
pub const SYS_RECVFROM: usize = 36;
pub const SYS_RECVMSG: usize = 37;
pub const SYS_LISTEN: usize = 38;
pub const SYS_SHUTDOWN: usize = 39;
pub const SYS_ACCEPT: usize = 40;

pub const SYS_GETSOCKNAME: usize = 41;
pub const SYS_GETPEERNAME: usize = 42;
pub const SYS_GETTIMEOFDAY: usize = 43;
pub const SYS_MMAP: usize = 44;
pub const SYS_MUNMAP: usize = 45;

pub const SYS_MPROTECT: usize = 46;
pub const SYS_FSTAT: usize = 47;
pub const SYS_GETCWD: usize = 48;
pub const SYS_GETPPID: usize = 49;
pub const SYS_GETPGID: usize = 50;

pub const SYS_FCNTL: usize = 51;
pub const SYS_FTRUNCATE: usize = 52;

#[derive(Debug)]
pub struct Syscall;

extern "C" {
    fn do_put_string(s: *const u8, front_color: u32, back_color: u32) -> usize;
}

#[no_mangle]
pub extern "C" fn syscall_init() -> i32 {
    kinfo!("Initializing syscall...");
    Syscall::init().expect("syscall init failed");
    kinfo!("Syscall init successfully!");
    return 0;
}

impl Syscall {
    /// 初始化系统调用
    pub fn init() -> Result<(), SystemError> {
        static INIT_FLAG: AtomicBool = AtomicBool::new(false);
        let prev = INIT_FLAG.swap(true, Ordering::SeqCst);
        if prev {
            panic!("Cannot initialize syscall more than once!");
        }
        return crate::arch::syscall::arch_syscall_init();
    }
    /// @brief 系统调用分发器，用于分发系统调用。
    ///
    /// 这个函数内，需要根据系统调用号，调用对应的系统调用处理函数。
    /// 并且，对于用户态传入的指针参数，需要在本函数内进行越界检查，防止访问到内核空间。
    pub fn handle(syscall_num: usize, args: &[usize], frame: &mut TrapFrame) -> usize {
        let r = match syscall_num {
            SYS_PUT_STRING => {
                Self::put_string(args[0] as *const u8, args[1] as u32, args[2] as u32)
            }
            SYS_OPEN => {
                let path: &CStr = unsafe { CStr::from_ptr(args[0] as *const c_char) };
                let path: Result<&str, core::str::Utf8Error> = path.to_str();
                let res = if path.is_err() {
                    Err(SystemError::EINVAL)
                } else {
                    let path: &str = path.unwrap();
                    let flags = args[1];
                    let open_flags: FileMode = FileMode::from_bits_truncate(flags as u32);

                    Self::open(path, open_flags)
                };

                res
            }
            SYS_CLOSE => {
                let fd = args[0];
                Self::close(fd)
            }
            SYS_READ => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let virt_addr: VirtAddr = VirtAddr::new(buf_vaddr);
                // 判断缓冲区是否来自用户态，进行权限校验
                let res = if frame.from_user() && verify_area(virt_addr, len as usize).is_err() {
                    // 来自用户态，而buffer在内核态，这样的操作不被允许
                    Err(SystemError::EPERM)
                } else {
                    let buf: &mut [u8] = unsafe {
                        core::slice::from_raw_parts_mut::<'static, u8>(buf_vaddr as *mut u8, len)
                    };

                    Self::read(fd, buf)
                };
                // kdebug!("sys read, fd: {}, len: {}, res: {:?}", fd, len, res);
                res
            }
            SYS_WRITE => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let virt_addr = VirtAddr::new(buf_vaddr);
                // 判断缓冲区是否来自用户态，进行权限校验
                let res = if frame.from_user() && verify_area(virt_addr, len as usize).is_err() {
                    // 来自用户态，而buffer在内核态，这样的操作不被允许
                    Err(SystemError::EPERM)
                } else {
                    let buf: &[u8] = unsafe {
                        core::slice::from_raw_parts::<'static, u8>(buf_vaddr as *const u8, len)
                    };

                    Self::write(fd, buf)
                };

                // kdebug!("sys write, fd: {}, len: {}, res: {:?}", fd, len, res);

                res
            }

            SYS_LSEEK => {
                let fd = args[0] as i32;
                let offset = args[1] as i64;
                let whence = args[2] as u32;

                let w = match whence {
                    SEEK_SET => Ok(SeekFrom::SeekSet(offset)),
                    SEEK_CUR => Ok(SeekFrom::SeekCurrent(offset)),
                    SEEK_END => Ok(SeekFrom::SeekEnd(offset)),
                    SEEK_MAX => Ok(SeekFrom::SeekEnd(0)),
                    _ => Err(SystemError::EINVAL),
                };

                let res = if w.is_err() {
                    Err(w.unwrap_err())
                } else {
                    let w = w.unwrap();
                    Self::lseek(fd, w)
                };
                // kdebug!("sys lseek, fd: {}, offset: {}, whence: {}, res: {:?}", fd, offset, whence, res);

                res
            }

            SYS_BRK => {
                let new_brk = VirtAddr::new(args[0]);
                Self::brk(new_brk).map(|vaddr| vaddr.data())
            }

            SYS_SBRK => {
                let increment = args[0] as isize;
                Self::sbrk(increment).map(|vaddr| vaddr.data())
            }

            SYS_REBOOT => Self::reboot(),

            SYS_CHDIR => {
                // Closure for checking arguments
                let chdir_check = |arg0: usize| {
                    if arg0 == 0 {
                        return Err(SystemError::EFAULT);
                    }
                    let path_ptr = arg0 as *const c_char;
                    let virt_addr = VirtAddr::new(path_ptr as usize);
                    // 权限校验
                    if path_ptr.is_null()
                        || (frame.from_user()
                            && verify_area(virt_addr, PAGE_2M_SIZE as usize).is_err())
                    {
                        return Err(SystemError::EINVAL);
                    }
                    let dest_path: &CStr = unsafe { CStr::from_ptr(path_ptr) };
                    let dest_path: &str = dest_path.to_str().map_err(|_| SystemError::EINVAL)?;
                    if dest_path.len() == 0 {
                        return Err(SystemError::EINVAL);
                    } else if dest_path.len() > MAX_PATHLEN as usize {
                        return Err(SystemError::ENAMETOOLONG);
                    }

                    return Ok(dest_path);
                };

                let r: Result<&str, SystemError> = chdir_check(args[0]);
                if r.is_err() {
                    Err(r.unwrap_err())
                } else {
                    Self::chdir(r.unwrap())
                }
            }

            SYS_GET_DENTS => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let virt_addr: VirtAddr = VirtAddr::new(buf_vaddr);
                // 判断缓冲区是否来自用户态，进行权限校验
                let res = if frame.from_user() && verify_area(virt_addr, len as usize).is_err() {
                    // 来自用户态，而buffer在内核态，这样的操作不被允许
                    Err(SystemError::EPERM)
                } else if buf_vaddr == 0 {
                    Err(SystemError::EFAULT)
                } else {
                    let buf: &mut [u8] = unsafe {
                        core::slice::from_raw_parts_mut::<'static, u8>(buf_vaddr as *mut u8, len)
                    };
                    Self::getdents(fd, buf)
                };

                res
            }

            SYS_EXECVE => {
                let path_ptr = args[0];
                let argv_ptr = args[1];
                let env_ptr = args[2];
                let virt_path_ptr = VirtAddr::new(path_ptr);
                let virt_argv_ptr = VirtAddr::new(argv_ptr);
                let virt_env_ptr = VirtAddr::new(env_ptr);
                // 权限校验
                if frame.from_user()
                    && (verify_area(virt_path_ptr, MAX_PATHLEN as usize).is_err()
                        || verify_area(virt_argv_ptr, PAGE_4K_SIZE as usize).is_err())
                    || verify_area(virt_env_ptr, PAGE_4K_SIZE as usize).is_err()
                {
                    Err(SystemError::EFAULT)
                } else {
                    Self::execve(
                        path_ptr as *const u8,
                        argv_ptr as *const *const u8,
                        env_ptr as *const *const u8,
                        frame,
                    )
                    .map(|_| 0)
                }
            }
            SYS_WAIT4 => {
                let pid = args[0] as pid_t;
                let wstatus = args[1] as *mut c_int;
                let options = args[2] as c_int;
                let rusage = args[3] as *mut c_void;
                let virt_wstatus = VirtAddr::new(wstatus as usize);
                let virt_rusage = VirtAddr::new(rusage as usize);
                // 权限校验
                // todo: 引入rusage之后，更正以下权限校验代码中，rusage的大小
                if frame.from_user()
                    && (verify_area(virt_wstatus, core::mem::size_of::<c_int>() as usize).is_err()
                        || verify_area(virt_rusage, PAGE_4K_SIZE as usize).is_err())
                {
                    Err(SystemError::EFAULT)
                } else {
                    Self::wait4(pid, wstatus, options, rusage)
                }
            }

            SYS_EXIT => {
                let exit_code = args[0];
                Self::exit(exit_code)
            }
            SYS_MKDIR => {
                let path_ptr = args[0] as *const c_char;
                let mode = args[1];
                let virt_path_ptr = VirtAddr::new(path_ptr as usize);
                let security_check = || {
                    if path_ptr.is_null()
                        || (frame.from_user()
                            && verify_area(virt_path_ptr, PAGE_2M_SIZE as usize).is_err())
                    {
                        return Err(SystemError::EINVAL);
                    }
                    let path: &CStr = unsafe { CStr::from_ptr(path_ptr) };
                    let path: &str = path.to_str().map_err(|_| SystemError::EINVAL)?.trim();

                    if path == "" {
                        return Err(SystemError::EINVAL);
                    }
                    return Ok(path);
                };

                let path = security_check();
                if path.is_err() {
                    Err(path.unwrap_err())
                } else {
                    Self::mkdir(path.unwrap(), mode)
                }
            }

            SYS_NANOSLEEP => {
                let req = args[0] as *const TimeSpec;
                let rem = args[1] as *mut TimeSpec;
                let virt_req = VirtAddr::new(req as usize);
                let virt_rem = VirtAddr::new(rem as usize);
                if frame.from_user()
                    && (verify_area(virt_req, core::mem::size_of::<TimeSpec>() as usize).is_err()
                        || verify_area(virt_rem, core::mem::size_of::<TimeSpec>() as usize)
                            .is_err())
                {
                    Err(SystemError::EFAULT)
                } else {
                    Self::nanosleep(req, rem)
                }
            }

            SYS_CLOCK => Self::clock(),
            SYS_PIPE => {
                let pipefd = args[0] as *mut c_int;
                match UserBufferWriter::new(
                    pipefd,
                    core::mem::size_of::<[c_int; 2]>(),
                    frame.from_user(),
                ) {
                    Err(e) => Err(e),
                    Ok(mut user_buffer) => match user_buffer.buffer::<i32>(0) {
                        Err(e) => Err(e),
                        Ok(pipefd) => Self::pipe(pipefd),
                    },
                }
            }

            SYS_UNLINK_AT => {
                let dirfd = args[0] as i32;
                let pathname = args[1] as *const c_char;
                let flags = args[2] as u32;
                let virt_pathname = VirtAddr::new(pathname as usize);
                if frame.from_user() && verify_area(virt_pathname, PAGE_4K_SIZE as usize).is_err() {
                    Err(SystemError::EFAULT)
                } else if pathname.is_null() {
                    Err(SystemError::EFAULT)
                } else {
                    let get_path = || {
                        let pathname: &CStr = unsafe { CStr::from_ptr(pathname) };

                        let pathname: &str = pathname.to_str().map_err(|_| SystemError::EINVAL)?;
                        if pathname.len() >= MAX_PATHLEN {
                            return Err(SystemError::ENAMETOOLONG);
                        }
                        return Ok(pathname.trim());
                    };
                    let pathname = get_path();
                    if pathname.is_err() {
                        Err(pathname.unwrap_err())
                    } else {
                        // kdebug!("sys unlinkat: dirfd: {}, pathname: {}", dirfd, pathname.as_ref().unwrap());
                        Self::unlinkat(dirfd, pathname.unwrap(), flags)
                    }
                }
            }
            SYS_KILL => {
                let pid = Pid::new(args[0]);
                let sig = args[1] as c_int;

                Self::kill(pid, sig)
            }

            SYS_SIGACTION => {
                let sig = args[0] as c_int;
                let act = args[1];
                let old_act = args[2];
                Self::sigaction(sig, act, old_act, frame.from_user())
            }

            SYS_RT_SIGRETURN => {
                // 由于目前signal机制的实现，与x86_64强关联，因此暂时在arch/x86_64/syscall.rs中调用
                // todo: 未来需要将signal机制与平台解耦
                todo!()
            }

            SYS_GETPID => Self::getpid().map(|pid| pid.into()),

            SYS_SCHED => Self::sched(frame.from_user()),
            SYS_DUP => {
                let oldfd: i32 = args[0] as c_int;
                Self::dup(oldfd)
            }
            SYS_DUP2 => {
                let oldfd: i32 = args[0] as c_int;
                let newfd: i32 = args[1] as c_int;
                Self::dup2(oldfd, newfd)
            }

            SYS_SOCKET => Self::socket(args[0], args[1], args[2]),
            SYS_SETSOCKOPT => {
                let optval = args[3] as *const u8;
                let optlen = args[4] as usize;
                let virt_optval = VirtAddr::new(optval as usize);
                // 验证optval的地址是否合法
                if verify_area(virt_optval, optlen as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    let data: &[u8] = unsafe { core::slice::from_raw_parts(optval, optlen) };
                    Self::setsockopt(args[0], args[1], args[2], data)
                }
            }
            SYS_GETSOCKOPT => {
                let optval = args[3] as *mut u8;
                let optlen = args[4] as *mut usize;
                let virt_optval = VirtAddr::new(optval as usize);
                let virt_optlen = VirtAddr::new(optlen as usize);
                let security_check = || {
                    // 验证optval的地址是否合法
                    if verify_area(virt_optval, PAGE_4K_SIZE as usize).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    // 验证optlen的地址是否合法
                    if verify_area(virt_optlen, core::mem::size_of::<u32>() as usize).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }
                    return Ok(());
                };
                let r = security_check();
                if r.is_err() {
                    Err(r.unwrap_err())
                } else {
                    Self::getsockopt(args[0], args[1], args[2], optval, optlen as *mut u32)
                }
            }

            SYS_CONNECT => {
                let addr = args[1] as *const SockAddr;
                let addrlen = args[2] as usize;
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证addr的地址是否合法
                if verify_area(virt_addr, addrlen as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    Self::connect(args[0], addr, addrlen)
                }
            }
            SYS_BIND => {
                let addr = args[1] as *const SockAddr;
                let addrlen = args[2] as usize;
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证addr的地址是否合法
                if verify_area(virt_addr, addrlen as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    Self::bind(args[0], addr, addrlen)
                }
            }

            SYS_SENDTO => {
                let buf = args[1] as *const u8;
                let len = args[2] as usize;
                let flags = args[3] as u32;
                let addr = args[4] as *const SockAddr;
                let addrlen = args[5] as usize;
                let virt_buf = VirtAddr::new(buf as usize);
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证buf的地址是否合法
                if verify_area(virt_buf, len as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else if verify_area(virt_addr, addrlen as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    let data: &[u8] = unsafe { core::slice::from_raw_parts(buf, len) };
                    Self::sendto(args[0], data, flags, addr, addrlen)
                }
            }

            SYS_RECVFROM => {
                let buf = args[1] as *mut u8;
                let len = args[2] as usize;
                let flags = args[3] as u32;
                let addr = args[4] as *mut SockAddr;
                let addrlen = args[5] as *mut usize;
                let virt_buf = VirtAddr::new(buf as usize);
                let virt_addrlen = VirtAddr::new(addrlen as usize);
                let virt_addr = VirtAddr::new(addr as usize);
                let security_check = || {
                    // 验证buf的地址是否合法
                    if verify_area(virt_buf, len as usize).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    // 验证addrlen的地址是否合法
                    if verify_area(virt_addrlen, core::mem::size_of::<u32>() as usize).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    if verify_area(virt_addr, core::mem::size_of::<SockAddr>() as usize).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }
                    return Ok(());
                };
                let r = security_check();
                if r.is_err() {
                    Err(r.unwrap_err())
                } else {
                    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
                    Self::recvfrom(args[0], buf, flags, addr, addrlen as *mut u32)
                }
            }

            SYS_RECVMSG => {
                let msg = args[1] as *mut crate::net::syscall::MsgHdr;
                let flags = args[2] as u32;
                match UserBufferWriter::new(
                    msg,
                    core::mem::size_of::<crate::net::syscall::MsgHdr>(),
                    true,
                ) {
                    Err(e) => Err(e),
                    Ok(mut user_buffer_writer) => {
                        match user_buffer_writer.buffer::<crate::net::syscall::MsgHdr>(0) {
                            Err(e) => Err(e),
                            Ok(buffer) => {
                                let msg = &mut buffer[0];
                                Self::recvmsg(args[0], msg, flags)
                            }
                        }
                    }
                }
            }

            SYS_LISTEN => Self::listen(args[0], args[1]),
            SYS_SHUTDOWN => Self::shutdown(args[0], args[1]),
            SYS_ACCEPT => Self::accept(args[0], args[1] as *mut SockAddr, args[2] as *mut u32),
            SYS_GETSOCKNAME => {
                Self::getsockname(args[0], args[1] as *mut SockAddr, args[2] as *mut u32)
            }
            SYS_GETPEERNAME => {
                Self::getpeername(args[0], args[1] as *mut SockAddr, args[2] as *mut u32)
            }
            SYS_GETTIMEOFDAY => {
                let timeval = args[0] as *mut PosixTimeval;
                let timezone_ptr = args[1] as *mut PosixTimeZone;
                Self::gettimeofday(timeval, timezone_ptr)
            }
            SYS_MMAP => {
                let len = page_align_up(args[1]);
                let virt_addr = VirtAddr::new(args[0] as usize);
                if verify_area(virt_addr, len as usize).is_err() {
                    Err(SystemError::EFAULT)
                } else {
                    Self::mmap(
                        VirtAddr::new(args[0]),
                        len,
                        args[2],
                        args[3],
                        args[4] as i32,
                        args[5],
                    )
                }
            }
            SYS_MUNMAP => {
                let addr = args[0];
                let len = page_align_up(args[1]);
                if addr & MMArch::PAGE_SIZE != 0 {
                    // The addr argument is not a multiple of the page size
                    Err(SystemError::EINVAL)
                } else {
                    Self::munmap(VirtAddr::new(addr), len)
                }
            }
            SYS_MPROTECT => {
                let addr = args[0];
                let len = page_align_up(args[1]);
                if addr & MMArch::PAGE_SIZE != 0 {
                    // The addr argument is not a multiple of the page size
                    Err(SystemError::EINVAL)
                } else {
                    Self::mprotect(VirtAddr::new(addr), len, args[2])
                }
            }

            SYS_GETCWD => {
                let buf = args[0] as *mut u8;
                let size = args[1] as usize;
                let security_check = || {
                    verify_area(VirtAddr::new(buf as usize), size)?;
                    return Ok(());
                };
                let r = security_check();
                if r.is_err() {
                    Err(r.unwrap_err())
                } else {
                    let buf = unsafe { core::slice::from_raw_parts_mut(buf, size) };
                    Self::getcwd(buf).map(|ptr| ptr.data())
                }
            }

            SYS_GETPGID => Self::getpgid(Pid::new(args[0])).map(|pid| pid.into()),

            SYS_GETPPID => Self::getppid().map(|pid| pid.into()),
            SYS_FSTAT => {
                let fd = args[0] as i32;
                let kstat = args[1] as *mut PosixKstat;
                let vaddr = VirtAddr::new(kstat as usize);
                // FIXME 由于c中的verify_area与rust中的verify_area重名，所以在引入时加了前缀区分
                // TODO 应该将用了c版本的verify_area都改为rust的verify_area
                match verify_area(vaddr, core::mem::size_of::<PosixKstat>()) {
                    Ok(_) => Self::fstat(fd, kstat),
                    Err(e) => Err(e),
                }
            }

            SYS_FCNTL => {
                let fd = args[0] as i32;
                let cmd: Option<FcntlCommand> =
                    <FcntlCommand as FromPrimitive>::from_u32(args[1] as u32);
                let arg = args[2] as i32;
                let res = if let Some(cmd) = cmd {
                    Self::fcntl(fd, cmd, arg)
                } else {
                    Err(SystemError::EINVAL)
                };

                // kdebug!("FCNTL: fd: {}, cmd: {:?}, arg: {}, res: {:?}", fd, cmd, arg, res);
                res
            }

            SYS_FTRUNCATE => {
                let fd = args[0] as i32;
                let len = args[1] as usize;
                let res = Self::ftruncate(fd, len);
                // kdebug!("FTRUNCATE: fd: {}, len: {}, res: {:?}", fd, len, res);
                res
            }

            _ => panic!("Unsupported syscall ID: {}", syscall_num),
        };

        let r = r.unwrap_or_else(|e| e.to_posix_errno() as usize);
        return r;
    }

    pub fn put_string(
        s: *const u8,
        front_color: u32,
        back_color: u32,
    ) -> Result<usize, SystemError> {
        return Ok(unsafe { do_put_string(s, front_color, back_color) });
    }

    pub fn reboot() -> Result<usize, SystemError> {
        cpu_reset();
    }
}
