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

        let kernel_addr = SockAddr::from(self.clone());
        let len = kernel_addr.len()? as usize;

        let to_write = core::cmp::min(len, user_len);

        // 使用 UserBufferWriter 的 buffer_protected 安全写入用户空间
        if to_write > 0 {
            let addr_bytes = unsafe {
                core::slice::from_raw_parts(
                    &kernel_addr as *const SockAddr as *const u8,
                    to_write as usize,
                )
            };
            let mut addr_writer = UserBufferWriter::new(addr as *mut u8, to_write, true)?;
            addr_writer
                .buffer_protected(0)?
                .write_to_user(0, addr_bytes)?;
        }

        // 写回实际需要的长度
        let mut addr_len_writer =
            UserBufferWriter::new(addr_len, core::mem::size_of::<u32>(), true)?;
        addr_len_writer
            .buffer_protected(0)?
            .write_one::<u32>(0, &(len as u32))?;

        Ok(())
    }
}
