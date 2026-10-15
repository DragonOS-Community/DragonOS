use crate::{
    net::{
        posix::SockAddr,
        socket::{
            netlink::addr::NetlinkSocketAddr, unix::UnixEndpoint, vsock::addr::VsockEndpoint,
        },
    },
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

pub use smoltcp::wire::IpEndpoint;

/// Socket ABI metadata, deliberately separate from the wire transport tuple.
#[derive(Debug, Clone, Copy)]
pub struct InetEndpoint {
    pub addr: smoltcp::wire::IpAddress,
    pub port: u16,
    pub scope_id: u32,
}

pub fn needs_scope_id(address: smoltcp::wire::IpAddress) -> bool {
    match address {
        smoltcp::wire::IpAddress::Ipv6(address) => {
            address.is_unicast_link_local()
                || (address.is_multicast() && matches!(address.octets()[1] & 0x0f, 1 | 2))
        }
        _ => false,
    }
}

impl InetEndpoint {
    pub(crate) fn from_ipv6_sockaddr(bytes: &[u8]) -> Result<Self, system_error::SystemError> {
        if bytes.len() < 24 {
            return Err(system_error::SystemError::EINVAL);
        }
        if u16::from_ne_bytes(bytes[..2].try_into().unwrap()) != 10 {
            return Err(system_error::SystemError::EAFNOSUPPORT);
        }
        Ok(Self {
            addr: smoltcp::wire::IpAddress::Ipv6(core::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(&bytes[8..24]).unwrap(),
            )),
            port: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
            scope_id: if bytes.len() >= 28 {
                u32::from_ne_bytes(bytes[24..28].try_into().unwrap())
            } else {
                0
            },
        })
    }

    pub fn wire(self) -> IpEndpoint {
        IpEndpoint::new(self.addr, self.port)
    }

    /// Only scoped IPv6 addresses expose an interface at the socket ABI.
    pub fn with_scope(mut self, ifindex: u32) -> Self {
        self.scope_id = if needs_scope_id(self.addr) {
            ifindex
        } else {
            0
        };
        self
    }

    pub fn scope_oif(self) -> Option<u32> {
        (needs_scope_id(self.addr) && self.scope_id != 0).then_some(self.scope_id)
    }
}

impl From<IpEndpoint> for InetEndpoint {
    fn from(endpoint: IpEndpoint) -> Self {
        Self {
            addr: endpoint.addr,
            port: endpoint.port,
            scope_id: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Endpoint {
    /// 未指定端点 (AF_UNSPEC) - 用于UDP断开连接
    Unspecified,
    /// 链路层端点
    LinkLayer(LinkLayerEndpoint),
    /// 网络层端点
    Ip(InetEndpoint),
    /// Unix域套接字端点
    Unix(UnixEndpoint),
    /// Netlink端点
    Netlink(NetlinkSocketAddr),
    /// Vsock端点
    Vsock(VsockEndpoint),
}

/// @brief 链路层端点 (对应 sockaddr_ll)
#[derive(Debug, Clone)]
pub struct LinkLayerEndpoint {
    /// 网卡的接口号 (sll_ifindex)
    pub interface: usize,
    /// MAC 地址 (sll_addr, 最多 8 字节，通常只使用 6 字节)
    pub addr: [u8; 8],
    /// 协议号 (sll_protocol, 网络字节序)
    pub protocol: u16,
    /// 硬件类型 (sll_hatype, 通常为 ARPHRD_ETHER = 1)
    pub hatype: u16,
    /// 包类型 (sll_pkttype)
    pub pkttype: u8,
    /// 地址长度 (sll_halen, 通常为 6)
    pub halen: u8,
}

impl LinkLayerEndpoint {
    /// @brief 创建一个链路层端点
    ///
    /// @param interface 网卡的接口号
    ///
    /// @return 返回创建的链路层端点
    pub fn new(interface: usize) -> Self {
        Self {
            interface,
            addr: [0u8; 8],
            protocol: 0,
            hatype: 1, // ARPHRD_ETHER
            pkttype: 0,
            halen: 6,
        }
    }

    /// 使用接口号和 MAC 地址创建链路层端点
    pub fn with_addr(interface: usize, addr: [u8; 8]) -> Self {
        Self {
            interface,
            addr,
            protocol: 0,
            hatype: 1,
            pkttype: 0,
            halen: 6,
        }
    }

    /// 创建完整的链路层端点
    pub fn full(
        interface: usize,
        addr: [u8; 8],
        protocol: u16,
        hatype: u16,
        pkttype: u8,
        halen: u8,
    ) -> Self {
        Self {
            interface,
            addr,
            protocol,
            hatype,
            pkttype,
            halen,
        }
    }
}

impl From<IpEndpoint> for Endpoint {
    fn from(endpoint: IpEndpoint) -> Self {
        Self::ip(endpoint)
    }
}

impl Endpoint {
    pub fn ip(endpoint: impl Into<InetEndpoint>) -> Self {
        Self::Ip(endpoint.into())
    }

    fn sockaddr_len(&self) -> Result<u32, system_error::SystemError> {
        match self {
            Endpoint::Unspecified => Ok(SockAddr::from(self.clone()).len()?),
            Endpoint::LinkLayer(_) => Ok(SockAddr::from(self.clone()).len()?),
            Endpoint::Ip(_) => Ok(SockAddr::from(self.clone()).len()?),
            Endpoint::Netlink(_) => Ok(SockAddr::from(self.clone()).len()?),
            Endpoint::Vsock(_) => Ok(SockAddr::from(self.clone()).len()?),
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
