use core::{any::Any, fmt::Debug, sync::atomic::AtomicUsize};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use common::poll_unit::{EPollItems, WaitQueue};
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
    unix::{SeqpacketSocket, StreamSocket},
    common::shutdown::Shutdown,
};

use super::{
    event_poll::{EPollEventType, EPollItem, EventPoll}, Endpoint,
};

pub mod inet;
pub mod unix;
pub mod define;
pub mod common;

pub use define::{AddressFamily, Options as SocketOptions, OptionsLevel as SocketOptionsLevel, Types as SocketTypes};

/* For setsockopt(2) */
// See: linux-5.19.10/include/uapi/asm-generic/socket.h#9
pub const SOL_SOCKET: u8 = 1;

// /// 根据地址族、socket类型和协议创建socket
// pub(super) fn new_unbound_socket(
//     address_family: AddressFamily,
//     socket_type: PosixSocketType,
//     protocol: Protocol,
// ) -> Result<Box<dyn Socket>, SystemError> {
//     let socket: Box<dyn Socket> = match address_family {
//         AddressFamily::Unix => match socket_type {
//             PosixSocketType::Stream => Box::new(StreamSocket::new(Options::default())),
//             PosixSocketType::SeqPacket => Box::new(SeqpacketSocket::new(Options::default())),
//             _ => {
//                 return Err(SystemError::EINVAL);
//             }
//         },
//         AddressFamily::INet => match socket_type {
//             PosixSocketType::Stream => Box::new(TcpSocket::new(Options::default())),
//             PosixSocketType::Datagram => Box::new(BoundUdp::new(Options::default())),
//             PosixSocketType::Raw => Box::new(RawSocket::new(protocol, Options::default())),
//             _ => {
//                 return Err(SystemError::EINVAL);
//             }
//         },
//         _ => {
//             return Err(SystemError::EAFNOSUPPORT);
//         }
//     };

//     Ok(socket)
// }

pub trait Socket: IndexNode {
    /// # `poll_unit`
    /// 获取socket的poll单元
    fn epoll_items(&self) -> &EPollItems;

    /// # `wait_queue`
    /// 获取socket的wait queue
    fn wait_queue(&self) -> &WaitQueue;

    /// # `on_iface_events`
    /// 通知socket iface事件更新
    fn on_iface_events(&self);

    /// # `connect` 
    /// 对应于POSIX的connect函数，用于连接到指定的远程服务器端点
    fn connect(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `bind` 
    /// 对应于POSIX的bind函数，用于绑定到本机指定的端点
    fn bind(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `shutdown`
    /// 对应于 POSIX 的 shutdown 函数，用于网络连接的可选关闭。
    fn shutdown(&self, _type: Shutdown) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `listen`
    /// 监听socket，仅用于stream socket
    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `accept`
    /// 接受连接，仅用于listening stream socket
    /// ## Block
    /// 如果没有连接到来，会阻塞
    fn accept(&self) -> Result<(Arc<dyn IndexNode>, Endpoint), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `endpoint`
    /// 获取绑定的端点
    fn endpoint(&self) -> Option<Endpoint> {
        None
    }

    /// # `peer_endpoint`
    /// 获取对端的端点
    fn peer_endpoint(&self) -> Option<Endpoint> {
        None
    }

    /// # `set_option`
    /// 对应 Posix `setsockopt` ，设置socket选项
    /// ## Parameters
    /// - level 选项的层次
    /// - optname 选项的名称
    /// - optval 选项的值
    /// ## Reference
    /// https://code.dragonos.org.cn/s?refs=sk_setsockopt&project=linux-6.6.21
    fn set_option(
        &self,
        _level: SocketOptionsLevel,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        warn!("setsockopt is not implemented");
        Ok(())
    }

    fn write_buffer(&self, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }
}

pub trait Poll {
    fn events(&self) -> EPollEventType;

    fn busy_wait<F, R>(&self, mask: EPollEventType, mut f: F) -> Result<R, SystemError>
    where
        F: FnMut() -> Result<R, SystemError>,
    {
        let wait_queue = WaitQueue::default();
        loop {
            match f() {
                Ok(r) => return Ok(r),
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    wait_queue.wait_for(mask);
                }
                Err(e) => return Err(e),
            }
        }
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
