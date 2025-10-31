use crate::{
    mm::{verify_area, VirtAddr},
    net::{
        posix::SockAddr,
        socket::{netlink::addr::NetlinkSocketAddr, unix::UnixEndpoint},
    },
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
        use system_error::SystemError::*;

        if addr.is_null() || addr_len.is_null() {
            return Ok(());
        }

        // 检查用户传入的地址是否合法
        verify_area(
            VirtAddr::new(addr as usize),
            core::mem::size_of::<SockAddr>(),
        )
        .map_err(|_| EFAULT)?;

        verify_area(
            VirtAddr::new(addr_len as usize),
            core::mem::size_of::<u32>(),
        )
        .map_err(|_| EFAULT)?;

        let kernel_addr = SockAddr::from(self.clone());
        let len = kernel_addr.len()?;

        unsafe {
            let to_write = core::cmp::min(len, *addr_len);
            if to_write > 0 {
                let buf = core::slice::from_raw_parts_mut(addr as *mut u8, to_write as usize);
                buf.copy_from_slice(core::slice::from_raw_parts(
                    &kernel_addr as *const SockAddr as *const u8,
                    to_write as usize,
                ));
            }
            *addr_len = len;
            return Ok(());
        }
    }
}
