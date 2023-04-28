use core::{
    fmt::{self, Debug},
    sync::atomic::AtomicUsize,
};

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};

use crate::{driver::net::NetDriver, kwarn, libs::rwlock::RwLock, syscall::SystemError};
use smoltcp::wire::IpEndpoint;

use self::socket::SocketMetadata;

pub mod endpoints;
pub mod net_core;
pub mod socket;
pub mod syscall;

lazy_static! {
    /// @brief 所有网络接口的列表
    pub static ref NET_DRIVERS: RwLock<BTreeMap<usize, Arc<dyn NetDriver>>> = RwLock::new(BTreeMap::new());
}

/// @brief 生成网络接口的id (全局自增)
pub fn generate_iface_id() -> usize {
    static IFACE_ID: AtomicUsize = AtomicUsize::new(0);
    return IFACE_ID
        .fetch_add(1, core::sync::atomic::Ordering::SeqCst)
        .into();
}

/// @brief 用于指定socket的关闭类型
/// 参考：https://pubs.opengroup.org/onlinepubs/9699919799/functions/shutdown.html
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum ShutdownType {
    ShutRd = 0,   // Disables further receive operations.
    ShutWr = 1,   // Disables further send operations.
    ShutRdwr = 2, // Disables further send and receive operations.
}

impl TryFrom<i32> for ShutdownType {
    type Error = SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        <Self as FromPrimitive>::from_i32(value).ok_or(SystemError::EINVAL)
    }
}

impl Into<i32> for ShutdownType {
    fn into(self) -> i32 {
        use num_traits::ToPrimitive;
        <Self as ToPrimitive>::to_i32(&self).unwrap()
    }
}

#[derive(Debug, Clone)]
pub enum Endpoint {
    /// 链路层端点
    LinkLayer(endpoints::LinkLayerEndpoint),
    /// 网络层端点
    Ip(Option<IpEndpoint>),
    // todo: 增加NetLink机制后，增加NetLink端点
}

pub trait Socket: Sync + Send + Debug {
    /// @brief 从socket中读取数据，如果socket是阻塞的，那么直到读取到数据才返回
    ///
    /// @param buf 读取到的数据存放的缓冲区
    ///
    /// @return - 成功：(返回读取的数据的长度，读取数据的端点).
    ///         - 失败：错误码
    fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint);

    /// @brief 向socket中写入数据。如果socket是阻塞的，那么直到写入的数据全部写入socket中才返回
    ///
    /// @param buf 要写入的数据
    /// @param to 要写入的目的端点，如果是None，那么写入的数据将会被丢弃
    ///
    /// @return 返回写入的数据的长度
    fn write(&self, buf: &[u8], to: Option<Endpoint>) -> Result<usize, SystemError>;

    /// @brief 对应于POSIX的connect函数，用于连接到指定的远程服务器端点
    ///
    /// It is used to establish a connection to a remote server.
    /// When a socket is connected to a remote server,
    /// the operating system will establish a network connection with the server
    /// and allow data to be sent and received between the local socket and the remote server.
    ///
    /// @param endpoint 要连接的端点
    ///
    /// @return 返回连接是否成功
    fn connect(&mut self, endpoint: Endpoint) -> Result<(), SystemError>;

    /// @brief 对应于POSIX的bind函数，用于绑定到本机指定的端点
    ///
    /// The bind() function is used to associate a socket with a particular IP address and port number on the local machine.
    ///
    /// @param endpoint 要绑定的端点
    ///
    /// @return 返回绑定是否成功
    fn bind(&mut self, _endpoint: Endpoint) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 对应于 POSIX 的 shutdown 函数，用于关闭socket。
    ///
    /// shutdown() 函数用于启动网络连接的正常关闭。
    /// 当在两个端点之间建立网络连接时，任一端点都可以通过调用其端点对象上的 shutdown() 函数来启动关闭序列。
    /// 此函数向远程端点发送关闭消息以指示本地端点不再接受新数据。
    ///
    /// @return 返回是否成功关闭
    fn shutdown(&self, _type: ShutdownType) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 对应于POSIX的listen函数，用于监听端点
    ///
    /// @param backlog 最大的等待连接数
    ///
    /// @return 返回监听是否成功
    fn listen(&mut self, _backlog: usize) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 对应于POSIX的accept函数，用于接受连接
    ///
    /// @param endpoint 对端的端点
    ///
    /// @return 返回接受连接是否成功
    fn accept(&mut self) -> Result<(Box<dyn Socket>, Endpoint), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 获取socket的端点
    ///
    /// @return 返回socket的端点
    fn endpoint(&self) -> Option<Endpoint> {
        return None;
    }

    /// @brief 获取socket的对端端点
    ///
    /// @return 返回socket的对端端点
    fn peer_endpoint(&self) -> Option<Endpoint> {
        return None;
    }

    /// @brief
    ///     The purpose of the poll function is to provide
    ///     a non-blocking way to check if a socket is ready for reading or writing,
    ///     so that you can efficiently handle multiple sockets in a single thread or event loop.
    ///
    /// @return (in, out, err)
    ///
    ///     The first boolean value indicates whether the socket is ready for reading. If it is true, then there is data available to be read from the socket without blocking.
    ///     The second boolean value indicates whether the socket is ready for writing. If it is true, then data can be written to the socket without blocking.
    ///     The third boolean value indicates whether the socket has encountered an error condition. If it is true, then the socket is in an error state and should be closed or reset
    ///
    fn poll(&self) -> (bool, bool, bool) {
        return (false, false, false);
    }

    /// @brief socket的ioctl函数
    ///
    /// @param cmd ioctl命令
    /// @param arg0 ioctl命令的第一个参数
    /// @param arg1 ioctl命令的第二个参数
    /// @param arg2 ioctl命令的第三个参数
    ///
    /// @return 返回ioctl命令的返回值
    fn ioctl(
        &self,
        _cmd: usize,
        _arg0: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<usize, SystemError> {
        return Ok(0);
    }

    /// @brief 获取socket的元数据
    fn metadata(&self) -> Result<SocketMetadata, SystemError>;

    fn box_clone(&self) -> Box<dyn Socket>;

    /// @brief 设置socket的选项
    ///
    /// @param level 选项的层次
    /// @param optname 选项的名称
    /// @param optval 选项的值
    ///
    /// @return 返回设置是否成功, 如果不支持该选项，返回ENOSYS
    fn setsockopt(
        &self,
        _level: usize,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        kwarn!("setsockopt is not implemented");
        return Err(SystemError::ENOSYS);
    }
}

impl Clone for Box<dyn Socket> {
    fn clone(&self) -> Box<dyn Socket> {
        self.box_clone()
    }
}

/// IP datagram encapsulated protocol.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[repr(u8)]
pub enum Protocol {
    HopByHop = 0x00,
    Icmp = 0x01,
    Igmp = 0x02,
    Tcp = 0x06,
    Udp = 0x11,
    Ipv6Route = 0x2b,
    Ipv6Frag = 0x2c,
    Icmpv6 = 0x3a,
    Ipv6NoNxt = 0x3b,
    Ipv6Opts = 0x3c,
    Unknown(u8),
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Protocol::HopByHop => write!(f, "Hop-by-Hop"),
            Protocol::Icmp => write!(f, "ICMP"),
            Protocol::Igmp => write!(f, "IGMP"),
            Protocol::Tcp => write!(f, "TCP"),
            Protocol::Udp => write!(f, "UDP"),
            Protocol::Ipv6Route => write!(f, "IPv6-Route"),
            Protocol::Ipv6Frag => write!(f, "IPv6-Frag"),
            Protocol::Icmpv6 => write!(f, "ICMPv6"),
            Protocol::Ipv6NoNxt => write!(f, "IPv6-NoNxt"),
            Protocol::Ipv6Opts => write!(f, "IPv6-Opts"),
            Protocol::Unknown(id) => write!(f, "0x{id:02x}"),
        }
    }
}

impl From<smoltcp::wire::IpProtocol> for Protocol {
    fn from(value: smoltcp::wire::IpProtocol) -> Self {
        let x: u8 = value.into();
        Protocol::from(x)
    }
}

impl From<u8> for Protocol {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Protocol::HopByHop,
            0x01 => Protocol::Icmp,
            0x02 => Protocol::Igmp,
            0x06 => Protocol::Tcp,
            0x11 => Protocol::Udp,
            0x2b => Protocol::Ipv6Route,
            0x2c => Protocol::Ipv6Frag,
            0x3a => Protocol::Icmpv6,
            0x3b => Protocol::Ipv6NoNxt,
            0x3c => Protocol::Ipv6Opts,
            _ => Protocol::Unknown(value),
        }
    }
}

impl Into<u8> for Protocol {
    fn into(self) -> u8 {
        match self {
            Protocol::HopByHop => 0x00,
            Protocol::Icmp => 0x01,
            Protocol::Igmp => 0x02,
            Protocol::Tcp => 0x06,
            Protocol::Udp => 0x11,
            Protocol::Ipv6Route => 0x2b,
            Protocol::Ipv6Frag => 0x2c,
            Protocol::Icmpv6 => 0x3a,
            Protocol::Ipv6NoNxt => 0x3b,
            Protocol::Ipv6Opts => 0x3c,
            Protocol::Unknown(id) => id,
        }
    }
}
