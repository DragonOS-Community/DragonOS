use core::{cmp::min, ffi::CStr};

use alloc::{boxed::Box, sync::Arc};
use num_traits::{FromPrimitive, ToPrimitive};
use smoltcp::wire;
use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        file::{File, FileMode},
        syscall::{IoVec, IoVecs},
        FileType,
    },
    libs::spinlock::SpinLockGuard,
    mm::{verify_area, VirtAddr},
    net::socket::{AddressFamily, SOL_SOCKET},
    process::ProcessManager,
    syscall::Syscall,
};

use super::{
    socket::{new_socket, PosixSocketType, Socket, SocketInode},
    Endpoint, Protocol, ShutdownType,
};

/// Flags for socket, socketpair, accept4
const SOCK_CLOEXEC: FileMode = FileMode::O_CLOEXEC;
const SOCK_NONBLOCK: FileMode = FileMode::O_NONBLOCK;

impl Syscall {
    /// @brief sys_socket系统调用的实际执行函数
    ///
    /// @param address_family 地址族
    /// @param socket_type socket类型
    /// @param protocol 传输协议
    pub fn socket(
        address_family: usize,
        socket_type: usize,
        protocol: usize,
    ) -> Result<usize, SystemError> {
        let address_family = AddressFamily::try_from(address_family as u16)?;
        let socket_type = PosixSocketType::try_from((socket_type & 0xf) as u8)?;
        let protocol = Protocol::from(protocol as u8);

        let socket = new_socket(address_family, socket_type, protocol)?;

        let socketinode: Arc<SocketInode> = SocketInode::new(socket);
        let f = File::new(socketinode, FileMode::O_RDWR)?;
        // 把socket添加到当前进程的文件描述符表中
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        let fd = fd_table_guard.alloc_fd(f, None).map(|x| x as usize);
        drop(fd_table_guard);
        return fd;
    }

    /// # sys_socketpair系统调用的实际执行函数
    ///
    /// ## 参数
    /// - `address_family`: 地址族
    /// - `socket_type`: socket类型
    /// - `protocol`: 传输协议
    /// - `fds`: 用于返回文件描述符的数组
    pub fn socketpair(
        address_family: usize,
        socket_type: usize,
        protocol: usize,
        fds: &mut [i32],
    ) -> Result<usize, SystemError> {
        let address_family = AddressFamily::try_from(address_family as u16)?;
        let socket_type = PosixSocketType::try_from((socket_type & 0xf) as u8)?;
        let protocol = Protocol::from(protocol as u8);

        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        // 创建一对socket
        let inode0 = SocketInode::new(new_socket(address_family, socket_type, protocol)?);
        let inode1 = SocketInode::new(new_socket(address_family, socket_type, protocol)?);

        // 进行pair
        unsafe {
            inode0
                .inner_no_preempt()
                .connect(Endpoint::Inode(Some(inode1.clone())))?;
            inode1
                .inner_no_preempt()
                .connect(Endpoint::Inode(Some(inode0.clone())))?;
        }

        fds[0] = fd_table_guard.alloc_fd(File::new(inode0, FileMode::O_RDWR)?, None)?;
        fds[1] = fd_table_guard.alloc_fd(File::new(inode1, FileMode::O_RDWR)?, None)?;

        drop(fd_table_guard);
        Ok(0)
    }

    /// @brief sys_setsockopt系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param level 选项级别
    /// @param optname 选项名称
    /// @param optval 选项值
    /// @param optlen optval缓冲区长度
    pub fn setsockopt(
        fd: usize,
        level: usize,
        optname: usize,
        optval: &[u8],
    ) -> Result<usize, SystemError> {
        let socket_inode: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        // 获取内层的socket（真正的数据）
        let socket: SpinLockGuard<Box<dyn Socket>> = socket_inode.inner();
        return socket.setsockopt(level, optname, optval).map(|_| 0);
    }

    /// @brief sys_getsockopt系统调用的实际执行函数
    ///
    /// 参考：https://man7.org/linux/man-pages/man2/setsockopt.2.html
    ///
    /// @param fd 文件描述符
    /// @param level 选项级别
    /// @param optname 选项名称
    /// @param optval 返回的选项值
    /// @param optlen 返回的optval缓冲区长度
    pub fn getsockopt(
        fd: usize,
        level: usize,
        optname: usize,
        optval: *mut u8,
        optlen: *mut u32,
    ) -> Result<usize, SystemError> {
        // 获取socket
        let optval = optval as *mut u32;
        let binding: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let socket = binding.inner();

        if level as u8 == SOL_SOCKET {
            let optname = PosixSocketOption::try_from(optname as i32)
                .map_err(|_| SystemError::ENOPROTOOPT)?;
            match optname {
                PosixSocketOption::SO_SNDBUF => {
                    // 返回发送缓冲区大小
                    unsafe {
                        *optval = socket.metadata().tx_buf_size as u32;
                        *optlen = core::mem::size_of::<u32>() as u32;
                    }
                    return Ok(0);
                }
                PosixSocketOption::SO_RCVBUF => {
                    // 返回默认的接收缓冲区大小
                    unsafe {
                        *optval = socket.metadata().rx_buf_size as u32;
                        *optlen = core::mem::size_of::<u32>() as u32;
                    }
                    return Ok(0);
                }
                _ => {
                    return Err(SystemError::ENOPROTOOPT);
                }
            }
        }
        drop(socket);

        // To manipulate options at any other level the
        // protocol number of the appropriate protocol controlling the
        // option is supplied.  For example, to indicate that an option is
        // to be interpreted by the TCP protocol, level should be set to the
        // protocol number of TCP.

        let posix_protocol =
            PosixIpProtocol::try_from(level as u16).map_err(|_| SystemError::ENOPROTOOPT)?;
        if posix_protocol == PosixIpProtocol::TCP {
            let optname = PosixTcpSocketOptions::try_from(optname as i32)
                .map_err(|_| SystemError::ENOPROTOOPT)?;
            match optname {
                PosixTcpSocketOptions::Congestion => return Ok(0),
                _ => {
                    return Err(SystemError::ENOPROTOOPT);
                }
            }
        }
        return Err(SystemError::ENOPROTOOPT);
    }

    /// @brief sys_connect系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn connect(fd: usize, addr: *const SockAddr, addrlen: usize) -> Result<usize, SystemError> {
        let endpoint: Endpoint = SockAddr::to_endpoint(addr, addrlen)?;
        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let mut socket = unsafe { socket.inner_no_preempt() };
        socket.connect(endpoint)?;
        Ok(0)
    }

    /// @brief sys_bind系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn bind(fd: usize, addr: *const SockAddr, addrlen: usize) -> Result<usize, SystemError> {
        let endpoint: Endpoint = SockAddr::to_endpoint(addr, addrlen)?;
        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let mut socket = unsafe { socket.inner_no_preempt() };
        socket.bind(endpoint)?;
        Ok(0)
    }

    /// @brief sys_sendto系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param buf 发送缓冲区
    /// @param flags 标志
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回发送的字节数，失败返回错误码
    pub fn sendto(
        fd: usize,
        buf: &[u8],
        _flags: u32,
        addr: *const SockAddr,
        addrlen: usize,
    ) -> Result<usize, SystemError> {
        let endpoint = if addr.is_null() {
            None
        } else {
            Some(SockAddr::to_endpoint(addr, addrlen)?)
        };

        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let socket = unsafe { socket.inner_no_preempt() };
        return socket.write(buf, endpoint);
    }

    /// @brief sys_recvfrom系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param buf 接收缓冲区
    /// @param flags 标志
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回接收的字节数，失败返回错误码
    pub fn recvfrom(
        fd: usize,
        buf: &mut [u8],
        _flags: u32,
        addr: *mut SockAddr,
        addrlen: *mut u32,
    ) -> Result<usize, SystemError> {
        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let socket = unsafe { socket.inner_no_preempt() };

        let (n, endpoint) = socket.read(buf);
        drop(socket);

        let n: usize = n?;

        // 如果有地址信息，将地址信息写入用户空间
        if !addr.is_null() {
            let sockaddr_in = SockAddr::from(endpoint);
            unsafe {
                sockaddr_in.write_to_user(addr, addrlen)?;
            }
        }
        return Ok(n);
    }

    /// @brief sys_recvmsg系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param msg MsgHdr
    /// @param flags 标志，暂时未使用
    ///
    /// @return 成功返回接收的字节数，失败返回错误码
    pub fn recvmsg(fd: usize, msg: &mut MsgHdr, _flags: u32) -> Result<usize, SystemError> {
        // 检查每个缓冲区地址是否合法，生成iovecs
        let mut iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };

        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let socket = unsafe { socket.inner_no_preempt() };

        let mut buf = iovs.new_buf(true);
        // 从socket中读取数据
        let (n, endpoint) = socket.read(&mut buf);
        drop(socket);

        let n: usize = n?;

        // 将数据写入用户空间的iovecs
        iovs.scatter(&buf[..n]);

        let sockaddr_in = SockAddr::from(endpoint);
        unsafe {
            sockaddr_in.write_to_user(msg.msg_name, &mut msg.msg_namelen)?;
        }
        return Ok(n);
    }

    /// @brief sys_listen系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param backlog 队列最大连接数
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn listen(fd: usize, backlog: usize) -> Result<usize, SystemError> {
        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let mut socket = unsafe { socket.inner_no_preempt() };
        socket.listen(backlog)?;
        return Ok(0);
    }

    /// @brief sys_shutdown系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param how 关闭方式
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn shutdown(fd: usize, how: usize) -> Result<usize, SystemError> {
        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let mut socket = unsafe { socket.inner_no_preempt() };
        socket.shutdown(ShutdownType::from_bits_truncate((how + 1) as u8))?;
        return Ok(0);
    }

    /// @brief sys_accept系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回新的文件描述符，失败返回错误码
    pub fn accept(fd: usize, addr: *mut SockAddr, addrlen: *mut u32) -> Result<usize, SystemError> {
        return Self::do_accept(fd, addr, addrlen, 0);
    }

    /// sys_accept4 - accept a connection on a socket
    ///
    ///
    /// If flags is 0, then accept4() is the same as accept().  The
    ///    following values can be bitwise ORed in flags to obtain different
    ///    behavior:
    ///
    /// - SOCK_NONBLOCK
    ///     Set the O_NONBLOCK file status flag on the open file
    ///     description (see open(2)) referred to by the new file
    ///     descriptor.  Using this flag saves extra calls to fcntl(2)
    ///     to achieve the same result.
    ///
    /// - SOCK_CLOEXEC
    ///     Set the close-on-exec (FD_CLOEXEC) flag on the new file
    ///     descriptor.  See the description of the O_CLOEXEC flag in
    ///     open(2) for reasons why this may be useful.
    pub fn accept4(
        fd: usize,
        addr: *mut SockAddr,
        addrlen: *mut u32,
        mut flags: u32,
    ) -> Result<usize, SystemError> {
        // 如果flags不合法，返回错误
        if (flags & (!(SOCK_CLOEXEC | SOCK_NONBLOCK)).bits()) != 0 {
            return Err(SystemError::EINVAL);
        }

        if SOCK_NONBLOCK != FileMode::O_NONBLOCK && ((flags & SOCK_NONBLOCK.bits()) != 0) {
            flags = (flags & !FileMode::O_NONBLOCK.bits()) | FileMode::O_NONBLOCK.bits();
        }

        return Self::do_accept(fd, addr, addrlen, flags);
    }

    fn do_accept(
        fd: usize,
        addr: *mut SockAddr,
        addrlen: *mut u32,
        flags: u32,
    ) -> Result<usize, SystemError> {
        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        // debug!("accept: socket={:?}", socket);
        let mut socket = unsafe { socket.inner_no_preempt() };
        // 从socket中接收连接
        let (new_socket, remote_endpoint) = socket.accept()?;
        drop(socket);

        // debug!("accept: new_socket={:?}", new_socket);
        // Insert the new socket into the file descriptor vector
        let new_socket: Arc<SocketInode> = SocketInode::new(new_socket);

        let mut file_mode = FileMode::O_RDWR;
        if flags & SOCK_NONBLOCK.bits() != 0 {
            file_mode |= FileMode::O_NONBLOCK;
        }
        if flags & SOCK_CLOEXEC.bits() != 0 {
            file_mode |= FileMode::O_CLOEXEC;
        }

        let new_fd = ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(File::new(new_socket, file_mode)?, None)?;
        // debug!("accept: new_fd={}", new_fd);
        if !addr.is_null() {
            // debug!("accept: write remote_endpoint to user");
            // 将对端地址写入用户空间
            let sockaddr_in = SockAddr::from(remote_endpoint);
            unsafe {
                sockaddr_in.write_to_user(addr, addrlen)?;
            }
        }
        return Ok(new_fd as usize);
    }

    /// @brief sys_getsockname系统调用的实际执行函数
    ///
    ///  Returns the current address to which the socket
    ///     sockfd is bound, in the buffer pointed to by addr.
    ///
    /// @param fd 文件描述符
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn getsockname(
        fd: usize,
        addr: *mut SockAddr,
        addrlen: *mut u32,
    ) -> Result<usize, SystemError> {
        if addr.is_null() {
            return Err(SystemError::EINVAL);
        }
        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let socket = socket.inner();
        let endpoint: Endpoint = socket.endpoint().ok_or(SystemError::EINVAL)?;
        drop(socket);

        let sockaddr_in = SockAddr::from(endpoint);
        unsafe {
            sockaddr_in.write_to_user(addr, addrlen)?;
        }
        return Ok(0);
    }

    /// @brief sys_getpeername系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn getpeername(
        fd: usize,
        addr: *mut SockAddr,
        addrlen: *mut u32,
    ) -> Result<usize, SystemError> {
        if addr.is_null() {
            return Err(SystemError::EINVAL);
        }

        let socket: Arc<SocketInode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let socket = socket.inner();
        let endpoint: Endpoint = socket.peer_endpoint().ok_or(SystemError::EINVAL)?;
        drop(socket);

        let sockaddr_in = SockAddr::from(endpoint);
        unsafe {
            sockaddr_in.write_to_user(addr, addrlen)?;
        }
        return Ok(0);
    }
}

// 参考资料： https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/netinet_in.h.html#tag_13_32
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; 108],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrLl {
    pub sll_family: u16,
    pub sll_protocol: u16,
    pub sll_ifindex: u32,
    pub sll_hatype: u16,
    pub sll_pkttype: u8,
    pub sll_halen: u8,
    pub sll_addr: [u8; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrNl {
    nl_family: u16,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrPlaceholder {
    pub family: u16,
    pub data: [u8; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union SockAddr {
    pub family: u16,
    pub addr_in: SockAddrIn,
    pub addr_un: SockAddrUn,
    pub addr_ll: SockAddrLl,
    pub addr_nl: SockAddrNl,
    pub addr_ph: SockAddrPlaceholder,
}

impl SockAddr {
    /// @brief 把用户传入的SockAddr转换为Endpoint结构体
    pub fn to_endpoint(addr: *const SockAddr, len: usize) -> Result<Endpoint, SystemError> {
        verify_area(
            VirtAddr::new(addr as usize),
            core::mem::size_of::<SockAddr>(),
        )
        .map_err(|_| SystemError::EFAULT)?;

        let addr = unsafe { addr.as_ref() }.ok_or(SystemError::EFAULT)?;
        unsafe {
            match AddressFamily::try_from(addr.family)? {
                AddressFamily::INet => {
                    if len < addr.len()? {
                        return Err(SystemError::EINVAL);
                    }

                    let addr_in: SockAddrIn = addr.addr_in;

                    let ip: wire::IpAddress = wire::IpAddress::from(wire::Ipv4Address::from_bytes(
                        &u32::from_be(addr_in.sin_addr).to_be_bytes()[..],
                    ));
                    let port = u16::from_be(addr_in.sin_port);

                    return Ok(Endpoint::Ip(Some(wire::IpEndpoint::new(ip, port))));
                }
                AddressFamily::Unix => {
                    let addr_un: SockAddrUn = addr.addr_un;

                    let path = CStr::from_bytes_until_nul(&addr_un.sun_path)
                        .map_err(|_| SystemError::EINVAL)?
                        .to_str()
                        .map_err(|_| SystemError::EINVAL)?;

                    let fd = Syscall::open(path.as_ptr(), FileMode::O_RDWR.bits(), 0o755, true)?;

                    let binding = ProcessManager::current_pcb().fd_table();
                    let fd_table_guard = binding.read();

                    let file = fd_table_guard.get_file_by_fd(fd as i32).unwrap();
                    if file.file_type() != FileType::Socket {
                        return Err(SystemError::ENOTSOCK);
                    }
                    let inode = file.inode();
                    let socketinode = inode.as_any_ref().downcast_ref::<Arc<SocketInode>>();

                    return Ok(Endpoint::Inode(socketinode.cloned()));
                }
                AddressFamily::Packet => {
                    // TODO: support packet socket
                    return Err(SystemError::EINVAL);
                }
                AddressFamily::Netlink => {
                    // TODO: support netlink socket
                    return Err(SystemError::EINVAL);
                }
                _ => {
                    return Err(SystemError::EINVAL);
                }
            }
        }
    }

    /// @brief 获取地址长度
    pub fn len(&self) -> Result<usize, SystemError> {
        let ret = match AddressFamily::try_from(unsafe { self.family })? {
            AddressFamily::INet => Ok(core::mem::size_of::<SockAddrIn>()),
            AddressFamily::Packet => Ok(core::mem::size_of::<SockAddrLl>()),
            AddressFamily::Netlink => Ok(core::mem::size_of::<SockAddrNl>()),
            AddressFamily::Unix => Err(SystemError::EINVAL),
            _ => Err(SystemError::EINVAL),
        };

        return ret;
    }

    /// @brief 把SockAddr的数据写入用户空间
    ///
    /// @param addr 用户空间的SockAddr的地址
    /// @param len 要写入的长度
    ///
    /// @return 成功返回写入的长度，失败返回错误码
    pub unsafe fn write_to_user(
        &self,
        addr: *mut SockAddr,
        addr_len: *mut u32,
    ) -> Result<usize, SystemError> {
        // 当用户传入的地址或者长度为空时，直接返回0
        if addr.is_null() || addr_len.is_null() {
            return Ok(0);
        }

        // 检查用户传入的地址是否合法
        verify_area(
            VirtAddr::new(addr as usize),
            core::mem::size_of::<SockAddr>(),
        )
        .map_err(|_| SystemError::EFAULT)?;

        verify_area(
            VirtAddr::new(addr_len as usize),
            core::mem::size_of::<u32>(),
        )
        .map_err(|_| SystemError::EFAULT)?;

        let to_write = min(self.len()?, *addr_len as usize);
        if to_write > 0 {
            let buf = core::slice::from_raw_parts_mut(addr as *mut u8, to_write);
            buf.copy_from_slice(core::slice::from_raw_parts(
                self as *const SockAddr as *const u8,
                to_write,
            ));
        }
        *addr_len = self.len()? as u32;
        return Ok(to_write);
    }
}

impl From<Endpoint> for SockAddr {
    fn from(value: Endpoint) -> Self {
        match value {
            Endpoint::Ip(ip_endpoint) => {
                // 未指定地址
                if ip_endpoint.is_none() {
                    return SockAddr {
                        addr_ph: SockAddrPlaceholder {
                            family: AddressFamily::Unspecified as u16,
                            data: [0; 14],
                        },
                    };
                }
                // 指定了地址
                let ip_endpoint = ip_endpoint.unwrap();
                match ip_endpoint.addr {
                    wire::IpAddress::Ipv4(ipv4_addr) => {
                        let addr_in = SockAddrIn {
                            sin_family: AddressFamily::INet as u16,
                            sin_port: ip_endpoint.port.to_be(),
                            sin_addr: u32::from_be_bytes(ipv4_addr.0).to_be(),
                            sin_zero: [0; 8],
                        };

                        return SockAddr { addr_in };
                    }
                    _ => {
                        unimplemented!("not support ipv6");
                    }
                }
            }

            Endpoint::LinkLayer(link_endpoint) => {
                let addr_ll = SockAddrLl {
                    sll_family: AddressFamily::Packet as u16,
                    sll_protocol: 0,
                    sll_ifindex: link_endpoint.interface as u32,
                    sll_hatype: 0,
                    sll_pkttype: 0,
                    sll_halen: 0,
                    sll_addr: [0; 8],
                };

                return SockAddr { addr_ll };
            }

            _ => {
                // todo: support other endpoint, like Netlink...
                unimplemented!("not support {value:?}");
            }
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MsgHdr {
    /// 指向一个SockAddr结构体的指针
    pub msg_name: *mut SockAddr,
    /// SockAddr结构体的大小
    pub msg_namelen: u32,
    /// scatter/gather array
    pub msg_iov: *mut IoVec,
    /// elements in msg_iov
    pub msg_iovlen: usize,
    /// 辅助数据
    pub msg_control: *mut u8,
    /// 辅助数据长度
    pub msg_controllen: usize,
    /// 接收到的消息的标志
    pub msg_flags: u32,
}

#[derive(Debug, Clone, Copy, FromPrimitive, ToPrimitive, PartialEq, Eq)]
pub enum PosixIpProtocol {
    /// Dummy protocol for TCP.
    IP = 0,
    /// Internet Control Message Protocol.
    ICMP = 1,
    /// Internet Group Management Protocol.
    IGMP = 2,
    /// IPIP tunnels (older KA9Q tunnels use 94).
    IPIP = 4,
    /// Transmission Control Protocol.
    TCP = 6,
    /// Exterior Gateway Protocol.
    EGP = 8,
    /// PUP protocol.
    PUP = 12,
    /// User Datagram Protocol.
    UDP = 17,
    /// XNS IDP protocol.
    IDP = 22,
    /// SO Transport Protocol Class 4.
    TP = 29,
    /// Datagram Congestion Control Protocol.
    DCCP = 33,
    /// IPv6-in-IPv4 tunnelling.
    IPv6 = 41,
    /// RSVP Protocol.
    RSVP = 46,
    /// Generic Routing Encapsulation. (Cisco GRE) (rfc 1701, 1702)
    GRE = 47,
    /// Encapsulation Security Payload protocol
    ESP = 50,
    /// Authentication Header protocol
    AH = 51,
    /// Multicast Transport Protocol.
    MTP = 92,
    /// IP option pseudo header for BEET
    BEETPH = 94,
    /// Encapsulation Header.
    ENCAP = 98,
    /// Protocol Independent Multicast.
    PIM = 103,
    /// Compression Header Protocol.
    COMP = 108,
    /// Stream Control Transport Protocol
    SCTP = 132,
    /// UDP-Lite protocol (RFC 3828)
    UDPLITE = 136,
    /// MPLS in IP (RFC 4023)
    MPLSINIP = 137,
    /// Ethernet-within-IPv6 Encapsulation
    ETHERNET = 143,
    /// Raw IP packets
    RAW = 255,
    /// Multipath TCP connection
    MPTCP = 262,
}

impl TryFrom<u16> for PosixIpProtocol {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match <Self as FromPrimitive>::from_u16(value) {
            Some(p) => Ok(p),
            None => Err(SystemError::EPROTONOSUPPORT),
        }
    }
}

impl From<PosixIpProtocol> for u16 {
    fn from(value: PosixIpProtocol) -> Self {
        <PosixIpProtocol as ToPrimitive>::to_u16(&value).unwrap()
    }
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, FromPrimitive, ToPrimitive, PartialEq, Eq)]
pub enum PosixSocketOption {
    SO_DEBUG = 1,
    SO_REUSEADDR = 2,
    SO_TYPE = 3,
    SO_ERROR = 4,
    SO_DONTROUTE = 5,
    SO_BROADCAST = 6,
    SO_SNDBUF = 7,
    SO_RCVBUF = 8,
    SO_SNDBUFFORCE = 32,
    SO_RCVBUFFORCE = 33,
    SO_KEEPALIVE = 9,
    SO_OOBINLINE = 10,
    SO_NO_CHECK = 11,
    SO_PRIORITY = 12,
    SO_LINGER = 13,
    SO_BSDCOMPAT = 14,
    SO_REUSEPORT = 15,
    SO_PASSCRED = 16,
    SO_PEERCRED = 17,
    SO_RCVLOWAT = 18,
    SO_SNDLOWAT = 19,
    SO_RCVTIMEO_OLD = 20,
    SO_SNDTIMEO_OLD = 21,

    SO_SECURITY_AUTHENTICATION = 22,
    SO_SECURITY_ENCRYPTION_TRANSPORT = 23,
    SO_SECURITY_ENCRYPTION_NETWORK = 24,

    SO_BINDTODEVICE = 25,

    /// 与SO_GET_FILTER相同
    SO_ATTACH_FILTER = 26,
    SO_DETACH_FILTER = 27,

    SO_PEERNAME = 28,

    SO_ACCEPTCONN = 30,

    SO_PEERSEC = 31,
    SO_PASSSEC = 34,

    SO_MARK = 36,

    SO_PROTOCOL = 38,
    SO_DOMAIN = 39,

    SO_RXQ_OVFL = 40,

    /// 与SCM_WIFI_STATUS相同
    SO_WIFI_STATUS = 41,
    SO_PEEK_OFF = 42,

    /* Instruct lower device to use last 4-bytes of skb data as FCS */
    SO_NOFCS = 43,

    SO_LOCK_FILTER = 44,
    SO_SELECT_ERR_QUEUE = 45,
    SO_BUSY_POLL = 46,
    SO_MAX_PACING_RATE = 47,
    SO_BPF_EXTENSIONS = 48,
    SO_INCOMING_CPU = 49,
    SO_ATTACH_BPF = 50,
    // SO_DETACH_BPF = SO_DETACH_FILTER,
    SO_ATTACH_REUSEPORT_CBPF = 51,
    SO_ATTACH_REUSEPORT_EBPF = 52,

    SO_CNX_ADVICE = 53,
    SCM_TIMESTAMPING_OPT_STATS = 54,
    SO_MEMINFO = 55,
    SO_INCOMING_NAPI_ID = 56,
    SO_COOKIE = 57,
    SCM_TIMESTAMPING_PKTINFO = 58,
    SO_PEERGROUPS = 59,
    SO_ZEROCOPY = 60,
    /// 与SCM_TXTIME相同
    SO_TXTIME = 61,

    SO_BINDTOIFINDEX = 62,

    SO_TIMESTAMP_OLD = 29,
    SO_TIMESTAMPNS_OLD = 35,
    SO_TIMESTAMPING_OLD = 37,
    SO_TIMESTAMP_NEW = 63,
    SO_TIMESTAMPNS_NEW = 64,
    SO_TIMESTAMPING_NEW = 65,

    SO_RCVTIMEO_NEW = 66,
    SO_SNDTIMEO_NEW = 67,

    SO_DETACH_REUSEPORT_BPF = 68,

    SO_PREFER_BUSY_POLL = 69,
    SO_BUSY_POLL_BUDGET = 70,

    SO_NETNS_COOKIE = 71,
    SO_BUF_LOCK = 72,
    SO_RESERVE_MEM = 73,
    SO_TXREHASH = 74,
    SO_RCVMARK = 75,
}

impl TryFrom<i32> for PosixSocketOption {
    type Error = SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match <Self as FromPrimitive>::from_i32(value) {
            Some(p) => Ok(p),
            None => Err(SystemError::EINVAL),
        }
    }
}

impl From<PosixSocketOption> for i32 {
    fn from(value: PosixSocketOption) -> Self {
        <PosixSocketOption as ToPrimitive>::to_i32(&value).unwrap()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum PosixTcpSocketOptions {
    /// Turn off Nagle's algorithm.
    NoDelay = 1,
    /// Limit MSS.
    MaxSegment = 2,
    /// Never send partially complete segments.
    Cork = 3,
    /// Start keeplives after this period.
    KeepIdle = 4,
    /// Interval between keepalives.
    KeepIntvl = 5,
    /// Number of keepalives before death.
    KeepCnt = 6,
    /// Number of SYN retransmits.
    Syncnt = 7,
    /// Lifetime for orphaned FIN-WAIT-2 state.
    Linger2 = 8,
    /// Wake up listener only when data arrive.
    DeferAccept = 9,
    /// Bound advertised window
    WindowClamp = 10,
    /// Information about this connection.
    Info = 11,
    /// Block/reenable quick acks.
    QuickAck = 12,
    /// Congestion control algorithm.
    Congestion = 13,
    /// TCP MD5 Signature (RFC2385).
    Md5Sig = 14,
    /// Use linear timeouts for thin streams
    ThinLinearTimeouts = 16,
    /// Fast retrans. after 1 dupack.
    ThinDupack = 17,
    /// How long for loss retry before timeout.
    UserTimeout = 18,
    /// TCP sock is under repair right now.
    Repair = 19,
    RepairQueue = 20,
    QueueSeq = 21,
    RepairOptions = 22,
    /// Enable FastOpen on listeners
    FastOpen = 23,
    Timestamp = 24,
    /// Limit number of unsent bytes in write queue.
    NotSentLowat = 25,
    /// Get Congestion Control (optional) info.
    CCInfo = 26,
    /// Record SYN headers for new connections.
    SaveSyn = 27,
    /// Get SYN headers recorded for connection.
    SavedSyn = 28,
    /// Get/set window parameters.
    RepairWindow = 29,
    /// Attempt FastOpen with connect.
    FastOpenConnect = 30,
    /// Attach a ULP to a TCP connection.
    ULP = 31,
    /// TCP MD5 Signature with extensions.
    Md5SigExt = 32,
    /// Set the key for Fast Open(cookie).
    FastOpenKey = 33,
    /// Enable TFO without a TFO cookie.
    FastOpenNoCookie = 34,
    ZeroCopyReceive = 35,
    /// Notify bytes available to read as a cmsg on read.
    /// 与TCP_CM_INQ相同
    INQ = 36,
    /// delay outgoing packets by XX usec
    TxDelay = 37,
}

impl TryFrom<i32> for PosixTcpSocketOptions {
    type Error = SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match <Self as FromPrimitive>::from_i32(value) {
            Some(p) => Ok(p),
            None => Err(SystemError::EINVAL),
        }
    }
}

impl From<PosixTcpSocketOptions> for i32 {
    fn from(val: PosixTcpSocketOptions) -> Self {
        <PosixTcpSocketOptions as ToPrimitive>::to_i32(&val).unwrap()
    }
}
