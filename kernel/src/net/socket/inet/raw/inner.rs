//! Raw socket 内部状态实现

use alloc::sync::Arc;
use smoltcp::wire::{IpAddress, IpProtocol, IpVersion};
use system_error::SystemError;

use crate::{
    libs::spinlock::SpinLock,
    net::socket::{inet::common::BoundInner, utils::extract_src_addr_from_ip_header},
    process::namespace::net_namespace::NetNamespace,
};

// 重新导出供 mod.rs 使用
pub use crate::net::socket::utils::extract_dst_addr_from_ip_header;

pub type SmolRawSocket = smoltcp::socket::raw::Socket<'static>;

// 重新导出供当前模块与外部调用者使用（保持兼容路径：inner::DEFAULT_*）。
pub use super::constants::{DEFAULT_METADATA_BUF_SIZE, DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE};

/// 未绑定的 raw socket
#[derive(Debug)]
pub struct UnboundRaw {
    socket: SmolRawSocket,
    ip_version: IpVersion,
    protocol: IpProtocol,
}

impl UnboundRaw {
    pub fn new(ip_version: IpVersion, protocol: IpProtocol) -> Self {
        let rx_buffer = smoltcp::socket::raw::PacketBuffer::new(
            vec![smoltcp::socket::raw::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_RX_BUF_SIZE],
        );
        let tx_buffer = smoltcp::socket::raw::PacketBuffer::new(
            vec![smoltcp::socket::raw::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; DEFAULT_TX_BUF_SIZE],
        );
        let socket = SmolRawSocket::new(ip_version, protocol, rx_buffer, tx_buffer);

        Self {
            socket,
            ip_version,
            protocol,
        }
    }

    /// 绑定到指定的本地地址
    pub fn bind(
        self,
        local_addr: IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<BoundRaw, SystemError> {
        let inner = BoundInner::bind(self.socket, &local_addr, netns)?;
        Ok(BoundRaw {
            inner,
            local_addr: Some(local_addr),
            remote_addr: SpinLock::new(None),
            ip_version: self.ip_version,
            protocol: self.protocol,
        })
    }

    /// Attach the raw socket to an iface for receiving packets, without binding
    /// to a specific local address (wildcard).
    pub fn bind_wildcard(self, netns: Arc<NetNamespace>) -> Result<BoundRaw, SystemError> {
        // Prefer loopback for localhost-focused workloads/tests; fall back to default iface.
        let iface: Arc<dyn crate::net::Iface> = if let Some(lo) = netns.loopback_iface() {
            lo
        } else if let Some(iface) = netns.default_iface() {
            iface
        } else {
            netns
                .device_list()
                .values()
                .next()
                .cloned()
                .ok_or(SystemError::ENODEV)?
        };

        let inner = BoundInner::bind_on_iface(self.socket, iface, netns.clone())?;
        Ok(BoundRaw {
            inner,
            local_addr: None,
            remote_addr: SpinLock::new(None),
            ip_version: self.ip_version,
            protocol: self.protocol,
        })
    }

    /// 绑定到临时地址（根据远程地址选择合适的本地地址）
    pub fn bind_ephemeral(
        self,
        remote: IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<BoundRaw, SystemError> {
        let (inner, address) = BoundInner::bind_ephemeral(self.socket, remote, netns)?;
        Ok(BoundRaw {
            inner,
            local_addr: Some(address),
            remote_addr: SpinLock::new(None),
            ip_version: self.ip_version,
            protocol: self.protocol,
        })
    }

    #[allow(dead_code)]
    pub fn ip_version(&self) -> IpVersion {
        self.ip_version
    }

    #[allow(dead_code)]
    pub fn protocol(&self) -> IpProtocol {
        self.protocol
    }
}

/// 已绑定的 raw socket
#[derive(Debug)]
pub struct BoundRaw {
    inner: BoundInner,
    local_addr: Option<IpAddress>,
    remote_addr: SpinLock<Option<IpAddress>>,
    ip_version: IpVersion,
    protocol: IpProtocol,
}

impl BoundRaw {
    pub fn with_mut_socket<F, T>(&self, f: F) -> T
    where
        F: FnMut(&mut SmolRawSocket) -> T,
    {
        self.inner.with_mut(f)
    }

    pub fn with_socket<F, T>(&self, f: F) -> T
    where
        F: Fn(&SmolRawSocket) -> T,
    {
        self.inner.with(f)
    }

    pub fn local_addr(&self) -> Option<IpAddress> {
        self.local_addr
    }

    pub fn remote_addr(&self) -> Option<IpAddress> {
        *self.remote_addr.lock()
    }

    pub fn connect(&self, remote: IpAddress) {
        self.remote_addr.lock().replace(remote);
    }

    /// 尝试接收数据包
    ///
    /// # 返回
    /// - `Ok((size, src_addr))`: 接收到的数据大小和源地址
    /// - `Err(SystemError::EAGAIN_OR_EWOULDBLOCK)`: 没有数据可读
    pub fn try_recv(
        &self,
        buf: &mut [u8],
        ip_version: IpVersion,
    ) -> Result<(usize, IpAddress), SystemError> {
        self.with_mut_socket(|socket| {
            if socket.can_recv() {
                match socket.recv() {
                    Ok(data) => {
                        let len = data.len().min(buf.len());
                        buf[..len].copy_from_slice(&data[..len]);

                        // 从 IP 头解析源地址
                        let src_addr = extract_src_addr_from_ip_header(data, ip_version).unwrap_or(
                            match ip_version {
                                IpVersion::Ipv4 => {
                                    IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED)
                                }
                                IpVersion::Ipv6 => {
                                    IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED)
                                }
                            },
                        );

                        Ok((len, src_addr))
                    }
                    Err(_) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                }
            } else {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            }
        })
    }

    /// 尝试发送数据包
    pub fn try_send(&self, buf: &[u8], to: Option<IpAddress>) -> Result<usize, SystemError> {
        let dest_addr = to.or(*self.remote_addr.lock());

        // 对于 raw socket，目标地址可以从数据包中获取
        // 如果没有指定目标地址且没有连接的远程地址，
        // smoltcp 会尝试从数据包的 IP 头中获取目标地址
        let _ = dest_addr; // 目前 smoltcp raw socket 不需要显式指定目标地址

        self.with_mut_socket(|socket| {
            if socket.can_send() {
                match socket.send_slice(buf) {
                    Ok(()) => Ok(buf.len()),
                    Err(_) => Err(SystemError::ENOBUFS),
                }
            } else {
                Err(SystemError::ENOBUFS)
            }
        })
    }

    pub fn inner(&self) -> &BoundInner {
        &self.inner
    }

    pub fn close(&self) {
        self.inner.release();
    }

    #[allow(dead_code)]
    pub fn ip_version(&self) -> IpVersion {
        self.ip_version
    }

    #[allow(dead_code)]
    pub fn protocol(&self) -> IpProtocol {
        self.protocol
    }
}

/// Raw socket 内部状态
#[derive(Debug)]
pub enum RawInner {
    /// 未绑定状态
    Unbound(UnboundRaw),
    /// 通配接收：已附着到某个 iface，但未绑定到具体本地地址
    Wildcard(BoundRaw),
    /// 已绑定状态
    Bound(BoundRaw),
}
