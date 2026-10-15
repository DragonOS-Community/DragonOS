//! Raw socket 内部状态实现

use alloc::sync::{Arc, Weak};
use smoltcp::wire::{IpAddress, IpProtocol, IpVersion};
use system_error::SystemError;

use crate::net::socket::{inet::common::BoundInner, utils::extract_src_addr_from_ip_header};

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
    #[expect(
        clippy::result_large_err,
        reason = "Failure must return the unchanged protocol owner without locked allocation"
    )]
    pub(crate) fn bind_prepared(
        self,
        local_addr: Option<IpAddress>,
        prepared: &mut super::super::common::registration::PreparedIfaceSocket,
        wrapper: Arc<super::RawSocket>,
    ) -> Result<BoundRaw, (Self, SystemError)> {
        let mut original = Some(self);
        match prepared.with_commit(|prepared| {
            let original = original.take().expect("one raw bind attempt");
            let inner = prepared.attach(original.socket);
            let bound = BoundRaw {
                inner,
                owner: Arc::downgrade(&wrapper),
                ip_version: original.ip_version,
                protocol: original.protocol,
            };
            wrapper.addresses.write().local = local_addr;
            prepared.publish(wrapper);
            bound
        }) {
            Ok(bound) => Ok(bound),
            Err(error) => Err((
                original
                    .take()
                    .expect("admission precedes protocol consumption"),
                error,
            )),
        }
    }

    pub(crate) fn try_new(
        ip_version: IpVersion,
        protocol: IpProtocol,
        memory: &crate::net::socket::buffer::SocketMemory,
    ) -> Result<Self, crate::mm::allocation::MemoryAllocationError> {
        let rx = crate::net::socket::buffer::try_owned_packet_buffer(
            memory,
            DEFAULT_METADATA_BUF_SIZE,
            DEFAULT_RX_BUF_SIZE,
        )?;
        let tx = crate::net::socket::buffer::try_owned_packet_buffer(
            memory,
            DEFAULT_METADATA_BUF_SIZE,
            DEFAULT_TX_BUF_SIZE,
        )?;
        Ok(Self {
            socket: SmolRawSocket::new(ip_version, protocol, rx, tx),
            ip_version,
            protocol,
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
    owner: Weak<super::RawSocket>,
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
        self.owner
            .upgrade()
            .and_then(|owner| owner.addresses.read().local)
    }

    pub fn remote_addr(&self) -> Option<IpAddress> {
        self.owner
            .upgrade()
            .and_then(|owner| owner.addresses.read().remote)
    }

    pub fn connect(&self, remote: IpAddress) {
        if let Some(owner) = self.owner.upgrade() {
            owner.addresses.write().remote = Some(remote);
        }
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
        let dest_addr = to.or(self.remote_addr());

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

    pub(crate) fn inner_mut(&mut self) -> &mut BoundInner {
        &mut self.inner
    }

    pub(crate) fn set_local_addr(&mut self, address: IpAddress) {
        if let Some(owner) = self.owner.upgrade() {
            owner.addresses.write().local = Some(address);
        }
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
