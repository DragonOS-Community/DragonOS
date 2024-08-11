use core::{any::Any, fmt::Debug, sync::atomic::AtomicUsize};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use log::warn;
use smoltcp::{
    iface::SocketSet,
    socket::{self, raw, tcp, udp},
};
use system_error::SystemError;

use crate::{
    arch::rand::rand,
    filesystem::vfs::{
        file::FileMode, syscall::ModeType, FilePrivateData, FileSystem, FileType, IndexNode,
        Metadata,
    },
    libs::{
        rwlock::{RwLock, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::EventWaitQueue,
    },
    process::{Pid, ProcessManager},
    sched::{schedule, SchedMode},
};

use self::{
    handle::GlobalSocketHandle,
    inet::{RawSocket, TcpSocket, BoundUdp},
    unix::{SeqpacketSocket, StreamSocket},
    common::shutdown::ShutdownType,
};

use super::{
    event_poll::{EPollEventType, EPollItem, EventPoll}, Endpoint,
};

pub mod handle;
pub mod inet;
pub mod unix;
pub mod ip_def;
pub mod poll_method;
pub mod define;
pub mod inode;
pub mod common;

pub use inode::SocketInode;
pub use define::{AddressFamily, Options as SocketOptions, OptionsLevel as SocketOptionsLevel, Types as SocketTypes};

lazy_static! {
    /// 所有socket的集合
    /// TODO: 优化这里，自己实现SocketSet！！！现在这样的话，不管全局有多少个网卡，每个时间点都只会有1个进程能够访问socket
    // pub static ref SOCKET_SET: SpinLock<SocketSet<'static >> = SpinLock::new(SocketSet::new(vec![]));
    /// SocketHandle表，每个SocketHandle对应一个SocketHandleItem，
    /// 注意！：在网卡中断中需要拿到这张表的🔓，在获取读锁时应该确保关中断避免死锁
    pub static ref HANDLE_MAP: RwLock<HashMap<GlobalSocketHandle, SocketHandleItem>> = RwLock::new(HashMap::new());
    /// 端口管理器
    pub static ref PORT_MANAGER: PortManager = PortManager::new();
}

/* For setsockopt(2) */
// See: linux-5.19.10/include/uapi/asm-generic/socket.h#9
pub const SOL_SOCKET: u8 = 1;

/// 根据地址族、socket类型和协议创建socket
pub(super) fn new_unbound_socket(
    address_family: AddressFamily,
    socket_type: PosixSocketType,
    protocol: Protocol,
) -> Result<Box<dyn Socket>, SystemError> {
    let socket: Box<dyn Socket> = match address_family {
        AddressFamily::Unix => match socket_type {
            PosixSocketType::Stream => Box::new(StreamSocket::new(Options::default())),
            PosixSocketType::SeqPacket => Box::new(SeqpacketSocket::new(Options::default())),
            _ => {
                return Err(SystemError::EINVAL);
            }
        },
        AddressFamily::INet => match socket_type {
            PosixSocketType::Stream => Box::new(TcpSocket::new(Options::default())),
            PosixSocketType::Datagram => Box::new(BoundUdp::new(Options::default())),
            PosixSocketType::Raw => Box::new(RawSocket::new(protocol, Options::default())),
            _ => {
                return Err(SystemError::EINVAL);
            }
        },
        _ => {
            return Err(SystemError::EAFNOSUPPORT);
        }
    };

    let handle_item = SocketHandleItem::new(Arc::downgrade(&socket.posix_item()));
    HANDLE_MAP
        .write_irqsave()
        .insert(socket.socket_handle(), handle_item);
    Ok(socket)
}

pub trait Socket: Sync + Send + Debug + Any {
    /// @brief 从socket中读取数据，如果socket是阻塞的，那么直到读取到数据才返回
    ///
    /// @param buf 读取到的数据存放的缓冲区
    ///
    /// @return - 成功：(返回读取的数据的长度，读取数据的端点).
    ///         - 失败：错误码
    fn read(&self, buf: &mut [u8]) -> Result<(usize, Endpoint), SystemError>;

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
    fn connect(&mut self, _endpoint: Endpoint) -> Result<(), SystemError>;

    /// @brief 对应于POSIX的bind函数，用于绑定到本机指定的端点
    ///
    /// The bind() function is used to associate a socket with a particular IP address and port number on the local machine.
    ///
    /// @param endpoint 要绑定的端点
    ///
    /// @return 返回绑定是否成功
    fn bind(&mut self, _endpoint: Endpoint) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief 对应于 POSIX 的 shutdown 函数，用于关闭socket。
    ///
    /// shutdown() 函数用于启动网络连接的正常关闭。
    /// 当在两个端点之间建立网络连接时，任一端点都可以通过调用其端点对象上的 shutdown() 函数来启动关闭序列。
    /// 此函数向远程端点发送关闭消息以指示本地端点不再接受新数据。
    ///
    /// @return 返回是否成功关闭
    fn shutdown(&mut self, _type: ShutdownType) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief 对应于POSIX的listen函数，用于监听端点
    ///
    /// @param backlog 最大的等待连接数
    ///
    /// @return 返回监听是否成功
    fn listen(&mut self, _backlog: usize) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief 对应于POSIX的accept函数，用于接受连接
    ///
    /// @param endpoint 对端的端点
    ///
    /// @return 返回接受连接是否成功
    fn accept(&mut self) -> Result<(Box<dyn Socket>, Endpoint), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief 获取socket的端点
    ///
    /// @return 返回socket的端点
    fn endpoint(&self) -> Option<Endpoint> {
        None
    }

    /// @brief 获取socket的对端端点
    ///
    /// @return 返回socket的对端端点
    fn peer_endpoint(&self) -> Option<Endpoint> {
        None
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
    fn poll(&self) -> EPollEventType {
        EPollEventType::empty()
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
        Ok(0)
    }

    /// @brief 获取socket的元数据
    fn metadata(&self) -> SocketMetadata;

    fn box_clone(&self) -> Box<dyn Socket>;

    /// @brief 设置socket的选项
    ///
    /// @param level 选项的层次
    /// @param optname 选项的名称
    /// @param optval 选项的值
    ///
    /// @return 返回设置是否成功, 如果不支持该选项，返回ENOSYS
    /// 
    /// ## See
    /// https://code.dragonos.org.cn/s?refs=sk_setsockopt&project=linux-6.6.21
    fn set_option(
        &self,
        _level: OptionsLevel,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        warn!("setsockopt is not implemented");
        Ok(())
    }

    fn socket_handle(&self) -> GlobalSocketHandle;

    fn write_buffer(&self, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn Any;

    fn as_any_mut(&mut self) -> &mut dyn Any;

    fn close(&mut self);

    // fn posix_item(&self) -> Arc<PosixSocketHandleItem>;
}

impl Clone for Box<dyn Socket> {
    fn clone(&self) -> Box<dyn Socket> {
        self.box_clone()
    }
}


bitflags! {
    /// @brief socket的选项
    #[derive(Default)]
    pub struct Options: u32 {
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
