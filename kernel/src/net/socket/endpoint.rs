use crate::{
    net::{
        posix::SockAddr,
        socket::{netlink::addr::NetlinkSocketAddr, unix::UnixEndpoint},
    },
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

pub use smoltcp::wire::IpEndpoint;

#[derive(Debug, Clone)]
pub enum Endpoint {
    /// 链路层端点
    LinkLayer(LinkLayerEndpoint),
    /// 网络层端点
    Ip(IpEndpoint),
    /// Unix域套接字端点
    Unix(UnixEndpoint),
    /// Netlink端点
    Netlink(NetlinkSocketAddr),
}

/// @brief 链路层端点
#[derive(Debug, Clone)]
pub struct LinkLayerEndpoint {
    /// 网卡的接口号
    pub interface: usize,
}

impl LinkLayerEndpoint {
    /// @brief 创建一个链路层端点
    ///
    /// @param interface 网卡的接口号
    ///
    /// @return 返回创建的链路层端点
    pub fn new(interface: usize) -> Self {
        Self { interface }
    }
}

impl From<IpEndpoint> for Endpoint {
    fn from(endpoint: IpEndpoint) -> Self {
        Self::Ip(endpoint)
    }
}

impl Endpoint {
    fn sockaddr_len(&self) -> Result<u32, system_error::SystemError> {
        match self {
            Endpoint::LinkLayer(_) => Ok(SockAddr::from(self.clone()).len()?),
            Endpoint::Ip(_) => Ok(SockAddr::from(self.clone()).len()?),
            Endpoint::Netlink(_) => Ok(SockAddr::from(self.clone()).len()?),
            Endpoint::Unix(unix) => {
                // Linux AF_UNIX getsockname/getpeername length semantics depend on the
                // effective address length, not always sizeof(sockaddr_un).
                //
                // For abstract namespace: len = offsetof(sun_path) + 1 + name_len.
                // For unnamed: len = offsetof(sun_path).
                // For filesystem: include terminating NUL.
                let base = core::mem::size_of::<u16>() as u32; // offsetof(sockaddr_un, sun_path)

                match unix {
                    UnixEndpoint::Unnamed => Ok(base),
                    UnixEndpoint::Abstract(name) => {
                        let copy_len = core::cmp::min(name.len(), 107);
                        Ok(base + 1 + (copy_len as u32))
                    }
                    UnixEndpoint::File(path) => {
                        let copy_len = core::cmp::min(path.len(), 107);
                        Ok(base + (copy_len as u32) + 1)
                    }
                }
            }
        }
    }

    /// 内部函数：将 sockaddr 写入用户空间缓冲区
    ///
    /// # 参数
    /// - `addr`: 用户空间的 sockaddr 缓冲区指针（调用方需保证非空）
    /// - `max_len`: 用户提供的缓冲区最大长度
    ///
    /// # 返回值
    /// 返回实际需要的 sockaddr 长度（即使缓冲区较小也返回完整长度，符合 Linux 语义）
    fn write_sockaddr_to_user(
        &self,
        addr: *mut SockAddr,
        max_len: usize,
    ) -> Result<u32, system_error::SystemError> {
        let kernel_addr = SockAddr::from(self.clone());
        let len = self.sockaddr_len()?;

        let to_write = core::cmp::min(len as usize, max_len);
        if to_write > 0 {
            let mut addr_writer = UserBufferWriter::new(addr as *mut u8, to_write, true)?;
            // 只能写入实际 sockaddr 的前 `to_write` 字节。
            // 注意：不能用 write_one::<SockAddr>()，因为 SockAddr 是 union，
            // 其大小通常大于某些地址族(如 AF_INET 的 sockaddr_in=16)。
            let kernel_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&kernel_addr as *const SockAddr) as *const u8,
                    len as usize,
                )
            };

            addr_writer
                .buffer_protected(0)?
                .write_to_user(0, &kernel_bytes[..to_write])?;
        }

        Ok(len)
    }

    /// 将端点地址写入用户空间（用于 getpeername/getsockname 等系统调用）
    ///
    /// # 参数
    /// - `addr`: 用户空间的 sockaddr 缓冲区指针
    /// - `addr_len`: 用户空间的长度指针（in/out 参数）
    pub fn write_to_user(
        &self,
        addr: *mut SockAddr,
        addr_len: *mut u32,
    ) -> Result<(), system_error::SystemError> {
        if addr.is_null() || addr_len.is_null() {
            return Ok(());
        }

        // 使用 UserBufferReader 读取用户提供的缓冲区长度
        let addr_len_reader = UserBufferReader::new(addr_len, core::mem::size_of::<u32>(), true)?;
        let user_len = addr_len_reader.buffer_protected(0)?.read_one::<u32>(0)? as usize;

        let len = self.write_sockaddr_to_user(addr, user_len)?;

        // 写回实际需要的长度
        let mut addr_len_writer =
            UserBufferWriter::new(addr_len, core::mem::size_of::<u32>(), true)?;
        addr_len_writer
            .buffer_protected(0)?
            .write_one::<u32>(0, &len)?;

        Ok(())
    }

    /// 将端点地址写入用户空间（用于 recvmsg 的 msghdr 风格）
    ///
    /// # 参数
    /// - `addr`: 用户空间的 sockaddr 缓冲区指针
    /// - `user_len`: 用户提供的缓冲区长度（按值传递）
    ///
    /// # 返回值
    /// 返回实际需要的 sockaddr 长度（即使缓冲区较小也返回完整长度，符合 Linux 语义）
    pub fn write_to_user_msghdr(
        &self,
        addr: *mut SockAddr,
        user_len: u32,
    ) -> Result<u32, system_error::SystemError> {
        if addr.is_null() {
            return Ok(user_len);
        }

        self.write_sockaddr_to_user(addr, user_len as usize)
    }
}
