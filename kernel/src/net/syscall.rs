use alloc::sync::Arc;
use log::debug;
use system_error::SystemError::{self, *};

use crate::{
    filesystem::vfs::file::{File, FileMode},
    process::ProcessManager,
    syscall::Syscall,
};

use super::socket::{self, unix::Unix, AddressFamily as AF, Endpoint};

pub use super::syscall_util::*;

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
        // 打印收到的参数
        // log::debug!(
        //     "socket: address_family={:?}, socket_type={:?}, protocol={:?}",
        //     address_family,
        //     socket_type,
        //     protocol
        // );
        let address_family = socket::AddressFamily::try_from(address_family as u16)?;
        let type_arg = SysArgSocketType::from_bits_truncate(socket_type as u32);
        let is_nonblock = type_arg.is_nonblock();
        let is_close_on_exec = type_arg.is_cloexec();
        let stype = socket::Type::try_from(type_arg)?;
        // log::debug!("type_arg {:?}  stype {:?}", type_arg, stype);

        let inode = socket::create_socket(
            address_family,
            stype,
            protocol as u32,
            is_nonblock,
            is_close_on_exec,
        )?;

        let file = File::new(inode, FileMode::O_RDWR)?;
        // 把socket添加到当前进程的文件描述符表中
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        let fd: Result<usize, SystemError> =
            fd_table_guard.alloc_fd(file, None).map(|x| x as usize);
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
        let address_family = AF::try_from(address_family as u16)?;
        let socket_type = SysArgSocketType::from_bits_truncate(socket_type as u32);
        let stype = socket::Type::try_from(socket_type)?;

        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        // check address family, only support AF_UNIX
        if address_family != AF::Unix {
            log::warn!(
                "only support AF_UNIX, {:?} with protocol {:?} is not supported",
                address_family,
                protocol
            );
            return Err(SystemError::EAFNOSUPPORT);
        }

        // 创建一对新的unix socket pair
        let (inode0, inode1) = Unix::new_pairs(stype)?;

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
        let sol = socket::OptionLevel::try_from(level as u32)?;
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        debug!("setsockopt: level={:?}", level);
        return socket.set_option(sol, optname, optval).map(|_| 0);
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
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(EBADF)?;

        let level = socket::OptionLevel::try_from(level as u32)?;

        use socket::OptionLevel as SOL;
        use socket::Options as SO;
        if matches!(level, SOL::SOCKET) {
            let optname = SO::try_from(optname as u32).map_err(|_| ENOPROTOOPT)?;
            match optname {
                SO::SNDBUF => {
                    // 返回发送缓冲区大小
                    unsafe {
                        *optval = socket.send_buffer_size() as u32;
                        *optlen = core::mem::size_of::<u32>() as u32;
                    }
                    return Ok(0);
                }
                SO::RCVBUF => {
                    // 返回默认的接收缓冲区大小
                    unsafe {
                        *optval = socket.recv_buffer_size() as u32;
                        *optlen = core::mem::size_of::<u32>() as u32;
                    }
                    return Ok(0);
                }
                _ => {
                    return Err(ENOPROTOOPT);
                }
            }
        }
        drop(socket);

        // To manipulate options at any other level the
        // protocol number of the appropriate protocol controlling the
        // option is supplied.  For example, to indicate that an option is
        // to be interpreted by the TCP protocol, level should be set to the
        // protocol number of TCP.

        if matches!(level, SOL::TCP) {
            use socket::inet::stream::TcpOption;
            let optname = TcpOption::try_from(optname as i32).map_err(|_| ENOPROTOOPT)?;
            match optname {
                TcpOption::Congestion => return Ok(0),
                _ => {
                    return Err(ENOPROTOOPT);
                }
            }
        }
        return Err(ENOPROTOOPT);
    }

    /// @brief sys_connect系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param addr SockAddr
    /// @param addrlen 地址长度
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn connect(fd: usize, addr: *const SockAddr, addrlen: u32) -> Result<usize, SystemError> {
        let endpoint: Endpoint = SockAddr::to_endpoint(addr, addrlen)?;
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
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
    pub fn bind(fd: usize, addr: *const SockAddr, addrlen: u32) -> Result<usize, SystemError> {
        // 打印收到的参数
        // log::debug!(
        //     "bind: fd={:?}, family={:?}, addrlen={:?}",
        //     fd,
        //     (unsafe { addr.as_ref().unwrap().family }),
        //     addrlen
        // );
        let endpoint: Endpoint = SockAddr::to_endpoint(addr, addrlen)?;
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        // log::debug!("bind: socket={:?}", socket);
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
        flags: u32,
        addr: *const SockAddr,
        addrlen: u32,
    ) -> Result<usize, SystemError> {
        let endpoint = if addr.is_null() {
            None
        } else {
            Some(SockAddr::to_endpoint(addr, addrlen)?)
        };

        let flags = socket::MessageFlag::from_bits_truncate(flags);

        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;

        if let Some(endpoint) = endpoint {
            return socket.send_to(buf, endpoint, flags);
        } else {
            return socket.send(buf, flags);
        }
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
        flags: u32,
        addr: *mut SockAddr,
        addr_len: *mut u32,
    ) -> Result<usize, SystemError> {
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        let flags = socket::MessageFlag::from_bits_truncate(flags);

        if addr.is_null() {
            let (n, _) = socket.recv_from(buf, flags, None)?;
            return Ok(n);
        }

        // address is not null
        let address = unsafe { addr.as_ref() }.ok_or(EINVAL)?;

        if unsafe { address.is_empty() } {
            let (recv_len, endpoint) = socket.recv_from(buf, flags, None)?;
            let sockaddr_in = SockAddr::from(endpoint);
            unsafe {
                sockaddr_in.write_to_user(addr, addr_len)?;
            }
            return Ok(recv_len);
        } else {
            // 从socket中读取数据
            let addr_len = *unsafe { addr_len.as_ref() }.ok_or(EINVAL)?;
            let address = SockAddr::to_endpoint(addr, addr_len)?;
            let (recv_len, _) = socket.recv_from(buf, flags, Some(address))?;
            return Ok(recv_len);
        };
    }

    /// @brief sys_recvmsg系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param msg MsgHdr
    /// @param flags 标志，暂时未使用
    ///
    /// @return 成功返回接收的字节数，失败返回错误码
    pub fn recvmsg(fd: usize, msg: &mut MsgHdr, flags: u32) -> Result<usize, SystemError> {
        todo!("recvmsg, fd={}, msg={:?}, flags={}", fd, msg, flags);
        // // 检查每个缓冲区地址是否合法，生成iovecs
        // let mut iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };

        // let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
        //     .get_socket(fd as i32)
        //     .ok_or(SystemError::EBADF)?;

        // let flags = socket::MessageFlag::from_bits_truncate(flags as u32);

        // let mut buf = iovs.new_buf(true);
        // // 从socket中读取数据
        // let recv_size = socket.recv_msg(&mut buf, flags)?;
        // drop(socket);

        // // 将数据写入用户空间的iovecs
        // iovs.scatter(&buf[..recv_size]);

        // // let sockaddr_in = SockAddr::from(endpoint);
        // // unsafe {
        // //     sockaddr_in.write_to_user(msg.msg_name, &mut msg.msg_namelen)?;
        // // }
        // return Ok(recv_size);
    }

    /// @brief sys_listen系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param backlog 队列最大连接数
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn listen(fd: usize, backlog: usize) -> Result<usize, SystemError> {
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        socket.listen(backlog).map(|_| 0)
    }

    /// @brief sys_shutdown系统调用的实际执行函数
    ///
    /// @param fd 文件描述符
    /// @param how 关闭方式
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn shutdown(fd: usize, how: usize) -> Result<usize, SystemError> {
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;
        socket.shutdown(socket::ShutdownTemp::from_how(how))?;
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
        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;

        // 从socket中接收连接
        let (new_socket, remote_endpoint) = socket.accept()?;
        drop(socket);

        // debug!("accept: new_socket={:?}", new_socket);
        // Insert the new socket into the file descriptor vector

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
        let endpoint = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?
            .get_name()?;

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

        let socket: Arc<socket::Inode> = ProcessManager::current_pcb()
            .get_socket(fd as i32)
            .ok_or(SystemError::EBADF)?;

        let endpoint: Endpoint = socket.get_peer_name()?;
        drop(socket);

        let sockaddr_in = SockAddr::from(endpoint);
        unsafe {
            sockaddr_in.write_to_user(addr, addrlen)?;
        }
        return Ok(0);
    }
}
