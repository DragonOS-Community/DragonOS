use crate::{
    filesystem::{
        epoll::EPollEventType,
        vfs::{fasync::FAsyncItems, FilePrivateData, IndexNode, InodeId, PollableInode},
    },
    libs::wait_queue::WaitQueue,
    net::{
        posix::MsgHdr,
        socket::common::{EPollItems, ShutdownBit},
    },
};
// use crate::filesystem::epoll::event_poll::EventPoll;
use alloc::sync::Arc;
use core::sync::atomic::AtomicUsize;
use system_error::SystemError;

use super::{
    endpoint::Endpoint,
    posix::{PMSG, PSOL},
};

/// # `Socket` methods
/// ## Reference
/// - [Posix standard](https://pubs.opengroup.org/onlinepubs/9699919799/)
pub trait Socket: PollableInode + IndexNode {
    /// Open-file refcount for this socket.
    ///
    /// Each `File` that references this socket (including those received via SCM_RIGHTS)
    /// corresponds to one successful `IndexNode::open()` and must be balanced by one
    /// `IndexNode::close()`. We use this counter to ensure `do_close()` runs only
    /// on the final close, matching Linux semantics and avoiding premature teardown.
    fn open_file_counter(&self) -> &AtomicUsize;

    /// # `wait_queue`
    /// 获取socket的wait queue
    fn wait_queue(&self) -> &WaitQueue;

    fn epoll_items(&self) -> &EPollItems;

    /// Get the fasync items for async I/O notification
    fn fasync_items(&self) -> &FAsyncItems;

    fn check_io_event(&self) -> EPollEventType;

    fn send_buffer_size(&self) -> usize;
    fn recv_buffer_size(&self) -> usize;

    /// # `recv_bytes_available`
    /// Get the number of bytes currently available to read from the socket.
    /// Returns 0 by default for socket types that don't track this.
    fn recv_bytes_available(&self) -> usize {
        0
    }

    /// # `accept`
    /// 接受连接，仅用于listening stream socket
    /// ## Block
    /// 如果没有连接到来，会阻塞
    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    /// # `bind`
    /// 对应于POSIX的bind函数，用于绑定到本机指定的端点
    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError>;

    /// # `close`
    /// 关闭socket
    fn do_close(&self) -> Result<(), SystemError>;

    /// # `connect`
    /// 对应于POSIX的connect函数，用于连接到指定的远程服务器端点
    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError>;

    /// Update the socket's nonblocking mode.
    ///
    /// Linux models O_NONBLOCK as a file status flag. DragonOS keeps some sockets'
    /// nonblocking state inside the socket object, so we provide this hook to sync
    /// fcntl(F_SETFL) changes.
    fn set_nonblocking(&self, _nonblocking: bool) {}

    // fnctl
    // freeaddrinfo
    // getaddrinfo
    // getnameinfo
    /// # `remote_endpoint`
    /// 获取对端的地址
    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `local_endpoint`
    /// 获取socket的地址
    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `get_option`
    /// 对应于 Posix `getsockopt` ，获取socket选项
    fn option(&self, _level: PSOL, _name: usize, _value: &mut [u8]) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `listen`
    /// 监听socket，仅用于stream socket
    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

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
    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError>;

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
    fn set_option(&self, _level: PSOL, _name: usize, _val: &[u8]) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// # `shutdown`
    fn shutdown(&self, _how: ShutdownBit) -> Result<(), SystemError> {
        // TODO 构建shutdown系统调用
        // set shutdown bit
        Err(SystemError::ENOSYS)
    }

    /// Socket-specific ioctl handler.
    ///
    /// By default sockets do not implement any ioctl commands.
    ///
    /// Note: caller is responsible for copying data to/from user space.
    fn ioctl(
        &self,
        _cmd: u32,
        _arg: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 唯一且稳定的 socket inode 号，由 socket 创建时分配
    fn socket_inode_id(&self) -> InodeId;

    // sockatmark
    // socket
    // socketpair
    /// # `write`
    fn write(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        self.send(buffer, PMSG::empty())
    }
}
