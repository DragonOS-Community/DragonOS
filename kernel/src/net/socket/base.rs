use crate::{
    filesystem::vfs::{IndexNode, PollableInode},
    libs::wait_queue::WaitQueue,
    net::posix::MsgHdr,
};
use alloc::sync::Arc;
use core::any::Any;
use core::fmt::Debug;
use system_error::SystemError;

use super::{
    common::shutdown::ShutdownBit,
    endpoint::Endpoint,
    posix::{PMSG, PSOL},
    // SocketInode,
};

/// # `Socket` methods
/// ## Reference
/// - [Posix standard](https://pubs.opengroup.org/onlinepubs/9699919799/)
pub trait Socket: PollableInode {
    /// # `wait_queue`
    /// 获取socket的wait queue
    fn wait_queue(&self) -> &WaitQueue;

    fn send_buffer_size(&self) -> usize;
    fn recv_buffer_size(&self) -> usize;
    /// # `accept`
    /// 接受连接，仅用于listening stream socket
    /// ## Block
    /// 如果没有连接到来，会阻塞
    fn accept(&self) -> Result<(Arc<dyn IndexNode>, Endpoint), SystemError>;

    /// # `bind`
    /// 对应于POSIX的bind函数，用于绑定到本机指定的端点
    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError>;

    /// # `close`
    /// 关闭socket
    fn do_close(&self) -> Result<(), SystemError>;

    /// # `connect`
    /// 对应于POSIX的connect函数，用于连接到指定的远程服务器端点
    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError>;

    // fnctl
    // freeaddrinfo
    // getaddrinfo
    // getnameinfo
    /// # `get_peer_name`
    /// 获取对端的地址
    fn get_peer_name(&self) -> Result<Endpoint, SystemError>;

    /// # `get_name`
    /// 获取socket的地址
    fn get_name(&self) -> Result<Endpoint, SystemError>;

    /// # `get_option`
    /// 对应于 Posix `getsockopt` ，获取socket选项
    fn get_option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError>;

    /// # `listen`
    /// 监听socket，仅用于stream socket
    fn listen(&self, backlog: usize) -> Result<(), SystemError>;

    // poll
    // pselect
    /// # `read`
    fn read(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.recv(buffer, PMSG::empty())
    }
    /// # `recv`
    /// 接收数据，`read` = `recv` with flags = 0
    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError>;

    /// # `recv_from`
    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError>;

    /// # `recv_msg`
    fn recv_msg(&self, msg: &mut MsgHdr, flags: PMSG) -> Result<usize, SystemError>;

    // select
    /// # `send`
    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    /// # `send_msg`
    fn send_msg(&self, msg: &MsgHdr, flags: PMSG) -> Result<usize, SystemError>;

    /// # `send_to`
    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError>;

    /// # `set_option`
    /// Posix `setsockopt` ，设置socket选项
    /// ## Parameters
    /// - level 选项的层次
    /// - name 选项的名称
    /// - value 选项的值
    /// ## Reference
    /// https://code.dragonos.org.cn/s?refs=sk_setsockopt&project=linux-6.6.21
    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError>;

    /// # `shutdown`
    fn shutdown(&self, how: usize) -> Result<(), SystemError>;

    // sockatmark
    // socket
    // socketpair
    /// # `write`
    fn write(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        self.send(buffer, PMSG::empty())
    }

    fn into_socket(this: Arc<Self>) -> Option<Arc<dyn Socket>>
    where
        Self: Sized,
    {
        Some(this)
    }
}
