use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::raw,
    wire::{IpAddress, IpProtocol, Ipv4Packet},
};

use crate::{libs::spinlock::SpinLock, syscall::SystemError};

use super::{Endpoint, Protocol, Socket};

lazy_static! {
    /// 所有socket的集合
    /// TODO: 优化这里，自己实现SocketSet！！！现在这样的话，不管全局有多少个网卡，每个时间点都只会有1个进程能够访问socket
    pub static ref SOCKET_SET: SpinLock<SocketSet<'static >> = SpinLock::new(SocketSet::new(vec![]));
}
/// @brief socket的句柄管理组件。
/// 它在smoltcp的SocketHandle上封装了一层，增加更多的功能。
/// 比如，在socket被关闭时，自动释放socket的资源，通知系统的其他组件。
#[derive(Debug)]
struct SocketHandler(SocketHandle);

impl SocketHandler {
    pub fn new(handler: SocketHandle) -> Self {
        Self(handler)
    }
}

impl Clone for SocketHandler {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for SocketHandler {
    fn drop(&mut self) {
        todo!()
    }
}

/// @brief socket的类型
#[derive(Debug)]
pub enum SocketType {
    /// 原始的socket
    Raw,
}

bitflags! {
    /// @brief socket的选项
    #[derive(Default)]
    pub struct SocketOptions: u32 {
        /// 是否阻塞
        const BLOCK = 1 << 0;
        /// 是否允许广播
        const BROADCAST = 1 << 1;
        /// 是否允许多播
        const MULTICAST = 1 << 2;
        /// 是否允许重用地址
        const REUSEADDR = 1 << 3;
        /// 是否允许重用端口
        const REUSEPORT = 1 << 4;
    }
}
#[derive(Debug)]
pub struct SocketMetadata {
    /// socket的类型
    socket_type: SocketType,
    /// 发送缓冲区的大小
    send_buf_size: usize,
    /// 接收缓冲区的大小
    recv_buf_size: usize,
    /// 元数据的缓冲区的大小
    metadata_buf_size: usize,
    /// socket的选项
    options: SocketOptions,
}

/// @brief 表示原始的socket
///
/// ref: https://man7.org/linux/man-pages/man7/raw.7.html
#[derive(Debug)]
pub struct RawSocket {
    handler: SocketHandler,
    /// 用户发送的数据包是否包含了IP头.
    /// 如果是true，用户发送的数据包，必须包含IP头。（即用户要自行设置IP头+数据）
    /// 如果是false，用户发送的数据包，不包含IP头。（即用户只要设置数据）
    header_included: bool,
    /// socket的选项
    options: SocketOptions,
}

impl RawSocket {
    /// 元数据的缓冲区的大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的发送缓冲区的大小
    pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;
    /// 默认的接收缓冲区的大小
    pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

    /// @brief 创建一个原始的socket
    ///
    /// @param protocol 协议号
    /// @param options socket的选项
    ///
    /// @return 返回创建的原始的socket
    pub fn new(protocol: Protocol, options: SocketOptions) -> Self {
        let tx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_TX_BUF_SIZE],
        );
        let rx_buffer = raw::PacketBuffer::new(
            vec![raw::PacketMetadata::EMPTY; Self::DEFAULT_METADATA_BUF_SIZE],
            vec![0; Self::DEFAULT_RX_BUF_SIZE],
        );
        let protocol: u8 = protocol.into();
        let socket = raw::Socket::new(
            smoltcp::wire::IpVersion::Ipv4,
            IpProtocol::from(protocol),
            tx_buffer,
            rx_buffer,
        );
        // 把socket添加到socket集合中，并得到socket的句柄
        let handler: SocketHandler = SocketHandler::new(SOCKET_SET.lock().add(socket));

        return Self {
            handler,
            header_included: false,
            options,
        };
    }
}

impl Socket for RawSocket {
    fn read(&self, buf: &mut [u8]) -> Result<(usize, Endpoint), SystemError> {
        loop {
            // 如何优化这里？
            let mut socket_set_guard = SOCKET_SET.lock();
            let socket = socket_set_guard.get_mut::<raw::Socket>(self.handler.0);

            match socket.recv_slice(buf) {
                Ok(len) => {
                    let packet = Ipv4Packet::new_unchecked(buf);
                    return Ok((
                        len,
                        Endpoint::Ip(smoltcp::wire::IpEndpoint {
                            addr: IpAddress::Ipv4(packet.src_addr()),
                            port: 0,
                        }),
                    ));
                }
                Err(smoltcp::socket::raw::RecvError::Exhausted) => {
                    if !self.options.contains(SocketOptions::BLOCK) {
                        // 如果是非阻塞的socket，就返回错误
                        return Err(SystemError::EAGAIN);
                    }
                }
            }
            drop(socket);
            drop(socket_set_guard);
        }
    }

    fn write(&self, buf: &[u8], to: Option<super::Endpoint>) -> Result<usize, SystemError> {
        // 如果用户发送的数据包，包含IP头，则直接发送
        if self.header_included {
            let mut socket_set_guard = SOCKET_SET.lock();
            let socket = socket_set_guard.get_mut::<raw::Socket>(self.handler.0);
            match socket.send_slice(buf) {
                Ok(len) => {
                    return Ok(buf.len());
                }
                Err(smoltcp::socket::raw::SendError::BufferFull) => {
                    return Err(SystemError::ENOBUFS);
                }
            }
        } else {
            // 如果用户发送的数据包，不包含IP头，则需要自己构造IP头

            if let Some(Endpoint::Ip(to)) = to {
                let mut socket_set_guard = SOCKET_SET.lock();
                let socket = socket_set_guard.get_mut::<raw::Socket>(self.handler.0);
                // 构造IP头
                // https://priv-code.longjin666.cn/xref/rCore/kernel/src/net/structs.rs?r=6a3a85ca#643
                todo!()
            } else {
                // 如果没有指定目的地址，则返回错误
                return Err(SystemError::ENOTCONN);
            }
        }

    }

    fn connect(&self, endpoint: super::Endpoint) -> Result<(), SystemError> {
        todo!()
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        todo!()
    }

    fn box_clone(&self) -> alloc::boxed::Box<dyn Socket> {
        todo!()
    }
}
