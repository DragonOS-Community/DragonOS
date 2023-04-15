use crate::{
    filesystem::vfs::syscall::IoVec, include::bindings::bindings::pt_regs, syscall::SystemError,
};

#[no_mangle]
pub extern "C" fn sys_socket(regs: &pt_regs) -> u64 {
    let address_family = regs.r8 as usize;
    let socket_type = regs.r9 as usize;
    let protocol = regs.r10 as usize;
    return do_socket(address_family, socket_type, protocol)
        .map(|x| x as u64)
        .unwrap_or_else(|e| e.to_posix_errno() as u64);
}

/// @brief sys_socket系统调用的实际执行函数
///
/// @param address_family 地址族
/// @param socket_type socket类型
/// @param protocol 传输协议
pub fn do_socket(
    address_family: usize,
    socket_type: usize,
    protocol: usize,
) -> Result<i64, SystemError> {
    todo!()
}

#[no_mangle]
pub extern "C" fn sys_setsockopt(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let level = regs.r9 as usize;
    let optname = regs.r10 as usize;
    let optval = regs.r11 as usize;
    let optlen = regs.r12 as usize;
    return do_setsockopt(fd, level, optname, optval as *const u8, optlen)
        .unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_setsockopt系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param level 选项级别
/// @param optname 选项名称
/// @param optval 选项值
/// @param optlen optval缓冲区长度
pub fn do_setsockopt(
    fd: usize,
    level: usize,
    optname: usize,
    optval: *const u8,
    optlen: usize,
) -> Result<i32, SystemError> {
    todo!()
}

#[no_mangle]
pub extern "C" fn sys_getsockopt(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let level = regs.r9 as usize;
    let optname = regs.r10 as usize;
    let optval = regs.r11 as usize;
    let optlen = regs.r12 as usize;
    return do_getsockopt(fd, level, optname, optval as *mut u8, optlen as *mut u32)
        .unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_getsockopt系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param level 选项级别
/// @param optname 选项名称
/// @param optval 返回的选项值
/// @param optlen 返回的optval缓冲区长度
pub fn do_getsockopt(
    fd: usize,
    level: usize,
    optname: usize,
    optval: *mut u8,
    optlen: *mut u32,
) -> Result<i32, SystemError> {
    todo!()
}

#[no_mangle]
pub extern "C" fn sys_connect(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let addr = regs.r9 as usize;
    let addrlen = regs.r10 as usize;
    return do_connect(fd, addr as *const SockAddr, addrlen).unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_connect系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param addr SockAddr
/// @param addrlen 地址长度
///
/// @return 成功返回0，失败返回错误码
pub fn do_connect(fd: usize, addr: *const SockAddr, addrlen: usize) -> Result<i32, SystemError> {
    todo!()
}

#[no_mangle]
pub extern "C" fn sys_bind(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let addr = regs.r9 as usize;
    let addrlen = regs.r10 as usize;
    return do_bind(fd, addr as *const SockAddr, addrlen).unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_bind系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param addr SockAddr
/// @param addrlen 地址长度
///
/// @return 成功返回0，失败返回错误码
pub fn do_bind(fd: usize, addr: *const SockAddr, addrlen: usize) -> Result<i32, SystemError> {
    todo!()
}

#[no_mangle]
pub extern "C" fn sys_sendto(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let buf = regs.r9 as usize;
    let len = regs.r10 as usize;
    let flags = regs.r11 as usize;
    let addr = regs.r12 as usize;
    let addrlen = regs.r13 as usize;
    return do_sendto(
        fd,
        buf as *const u8,
        len,
        flags,
        addr as *const SockAddr,
        addrlen,
    )
    .unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_sendto系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param buf 发送缓冲区
/// @param len 发送缓冲区长度
/// @param flags 标志
/// @param addr SockAddr
/// @param addrlen 地址长度
///
/// @return 成功返回发送的字节数，失败返回错误码
pub fn do_sendto(
    fd: usize,
    buf: *const u8,
    len: usize,
    flags: usize,
    addr: *const SockAddr,
    addrlen: usize,
) -> Result<i32, SystemError> {
    todo!()
}

pub extern "C" fn sys_recvfrom(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let buf = regs.r9 as usize;
    let len = regs.r10 as usize;
    let flags = regs.r11 as usize;
    let addr = regs.r12 as usize;
    let addrlen = regs.r13 as usize;
    return do_recvfrom(
        fd,
        buf as *mut u8,
        len,
        flags,
        addr as *mut SockAddr,
        addrlen as *mut u32,
    )
    .unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_recvfrom系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param buf 接收缓冲区
/// @param len 接收缓冲区长度
/// @param flags 标志
/// @param addr SockAddr
/// @param addrlen 地址长度
///
/// @return 成功返回接收的字节数，失败返回错误码
pub fn do_recvfrom(
    fd: usize,
    buf: *mut u8,
    len: usize,
    flags: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
) -> Result<i32, SystemError> {
    todo!()
}

pub extern "C" fn sys_recvmsg(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let msg = regs.r9 as usize;
    let flags = regs.r10 as usize;
    return do_recvmsg(fd, msg as *mut MsgHdr, flags).unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_recvmsg系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param msg MsgHdr
/// @param flags 标志
///
/// @return 成功返回接收的字节数，失败返回错误码
pub fn do_recvmsg(fd: usize, msg: *mut MsgHdr, flags: usize) -> Result<i32, SystemError> {
    todo!()
}

pub extern "C" fn sys_listen(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let backlog = regs.r9 as usize;
    return do_listen(fd, backlog).unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_listen系统调用的实际执行函数
///
/// @param fd 文件描述符
/// @param backlog 最大连接数
///
/// @return 成功返回0，失败返回错误码
pub fn do_listen(fd: usize, backlog: usize) -> Result<i32, SystemError> {
    todo!()
}

pub extern "C" fn sys_shutdown(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let how = regs.r9 as usize;
    return do_shutdown(fd, how).unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_shutdown系统调用的实际执行函数
/// 
/// @param fd 文件描述符
/// @param how 关闭方式
/// 
/// @return 成功返回0，失败返回错误码
pub fn do_shutdown(fd: usize, how: usize) -> Result<i32, SystemError> {
    todo!()
}

pub extern "C" fn sys_accept(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let addr = regs.r9 as usize;
    let addrlen = regs.r10 as usize;
    return do_accept(fd, addr as *mut SockAddr, addrlen as *mut u32)
        .unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_accept系统调用的实际执行函数
/// 
/// @param fd 文件描述符
/// @param addr SockAddr
/// @param addrlen 地址长度
/// 
/// @return 成功返回新的文件描述符，失败返回错误码
pub fn do_accept(
    fd: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
) -> Result<i32, SystemError> {
    todo!()
}

pub extern "C" fn sys_getsockname(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let addr = regs.r9 as usize;
    let addrlen = regs.r10 as usize;
    return do_getsockname(fd, addr as *mut SockAddr, addrlen as *mut u32)
        .unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_getsockname系统调用的实际执行函数
/// 
/// @param fd 文件描述符
/// @param addr SockAddr
/// @param addrlen 地址长度
/// 
/// @return 成功返回0，失败返回错误码
pub fn do_getsockname(
    fd: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
) -> Result<i32, SystemError> {
    todo!()
}

pub extern "C" fn sys_getpeername(regs: &pt_regs) -> i32 {
    let fd = regs.r8 as usize;
    let addr = regs.r9 as usize;
    let addrlen = regs.r10 as usize;
    return do_getpeername(fd, addr as *mut SockAddr, addrlen as *mut u32)
        .unwrap_or_else(|e| e.to_posix_errno());
}

/// @brief sys_getpeername系统调用的实际执行函数
/// 
/// @param fd 文件描述符
/// @param addr SockAddr
/// @param addrlen 地址长度
/// 
/// @return 成功返回0，失败返回错误码
pub fn do_getpeername(
    fd: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
) -> Result<i32, SystemError> {
    todo!()
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

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MsgHdr {
    pub msg_name: *mut u8,
    pub msg_namelen: u32,
    pub msg_iov: *mut IoVec,
    pub msg_iovlen: u32,
    pub msg_control: *mut u8,
    pub msg_controllen: u32,
    pub msg_flags: u32,
}
