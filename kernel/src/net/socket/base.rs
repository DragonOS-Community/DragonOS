use crate::{
    filesystem::{
        epoll::EPollEventType,
        page_cache::PageCache,
        vfs::{
            fasync::FAsyncItems, FilePrivateData, FileSystem, IndexNode, InodeId, PollableInode,
        },
    },
    libs::wait_queue::WaitQueue,
    net::{
        posix::{MsgHdr, SockAddr},
        socket::common::{EPollItems, ShutdownBit},
    },
};
// use crate::filesystem::epoll::event_poll::EventPoll;
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::AtomicUsize;
use system_error::SystemError;

use crate::syscall::user_access::UserBufferReader;

use super::{
    endpoint::Endpoint,
    posix::{PMSG, PSOL},
};

/// Layout information for mmap-backed sockets (e.g. AF_PACKET TPACKET rings).
///
/// Returned by [`Socket::mmap_layout`] to supply everything the mmap page-fault
/// path needs in a single call, avoiding repeated locking.
pub struct SocketMmapLayout {
    /// Page cache backing the mapped pages.
    pub page_cache: Arc<PageCache>,
    /// Fake filesystem whose `fault`/`map_pages` delegate to `PageFaultHandler`.
    pub fs: Arc<dyn FileSystem>,
    /// Logical size of the mapped region in bytes (for `filemap_fault` bounds check).
    pub size: usize,
}

/// # `Socket` methods
/// ## Reference
/// - [Posix standard](https://pubs.opengion.org/onlinepubs/9699919799/)

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

    /// # `send_bytes_available`
    /// Get the number of bytes currently available to write to the socket.
    /// Returns 0 by default for socket types that don't track this.
    fn send_bytes_available(&self) -> Result<usize, SystemError> {
        Err(SystemError::ENOTTY)
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

    /// `recvfrom(2)` 是否应输出源地址到 addr/addrlen。
    ///
    /// 默认行为是写回源地址（若调用者提供了 addr/addrlen）。stream socket（如 TCP）
    /// 应覆盖为 `Ignore` 以符合 Linux/gVisor 语义。
    fn recvfrom_addr_behavior(&self) -> super::RecvFromAddrBehavior {
        super::RecvFromAddrBehavior::Write
    }

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

    /// 直接把 `read(2)` 数据写入用户缓冲区。
    ///
    fn read_to_user_buffer(
        &self,
        user_buffer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    ) -> Result<usize, SystemError>;

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

    /// Validate a send buffer length before copying the user payload.
    ///
    /// Message-oriented sockets should reject impossible message sizes here so
    /// syscall code does not allocate a kernel buffer that the socket will later
    /// reject as `EMSGSIZE`.
    fn validate_send_buffer_len(
        &self,
        _len: usize,
        _address: Option<&Endpoint>,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    /// Send directly from a validated user buffer.
    ///
    /// The default keeps message-oriented socket semantics by materializing the
    /// whole message after socket-specific length validation. Stream sockets can
    /// override this to copy and send bounded chunks.
    fn send_user_buffer(
        &self,
        reader: &UserBufferReader<'_>,
        len: usize,
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<usize, SystemError> {
        self.validate_send_buffer_len(len, address.as_ref())?;

        let data = copy_user_buffer_to_vec(reader, len)?;
        if let Some(endpoint) = address {
            self.send_to(&data, flags, endpoint)
        } else {
            self.send(&data, flags)
        }
    }

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

    /// 验证 sendto/sendmsg 的目标地址
    ///
    /// 用于在发送数据前验证用户提供的目标地址是否有效。
    /// 默认实现不做任何检查，各 socket 类型可根据需要覆盖此方法。
    ///
    /// # 参数
    /// - `addr`: 用户提供的目标地址指针（可能为 null）
    /// - `addrlen`: 地址长度
    ///
    /// # 返回
    /// - `Ok(())`: 地址有效
    /// - `Err(SystemError)`: 地址无效
    fn validate_sendto_addr(
        &self,
        _addr: *const SockAddr,
        _addrlen: u32,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    // sockatmark
    // socket
    // socketpair
    /// # `write`
    fn write(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        self.send(buffer, PMSG::empty())
    }
    /// Returns mmap layout for mmap-backed sockets.
    ///
    /// Default `None` — most sockets don't support mmap. AF_PACKET overrides this
    /// to expose the TPACKET ring buffer to the page-fault path.
    fn mmap_layout(&self) -> Option<SocketMmapLayout> {
        None
    }
}

pub(crate) fn read_to_user_buffer_via_kernel_buf<S: Socket + ?Sized>(
    socket: &S,
    user_buffer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    kernel_buf_len: usize,
) -> Result<usize, SystemError> {
    if user_buffer.is_empty() {
        return Ok(0);
    }

    let scratch_len = core::cmp::min(kernel_buf_len.max(1), user_buffer.len());
    let mut kbuf = alloc::vec![0u8; scratch_len];
    let n = socket.read(&mut kbuf)?;
    user_buffer.write_to_user(0, &kbuf[..n])?;
    Ok(n)
}

pub(crate) fn copy_user_buffer_to_vec(
    reader: &UserBufferReader<'_>,
    len: usize,
) -> Result<Vec<u8>, SystemError> {
    let mut data = Vec::new();
    data.try_reserve(len).map_err(|_| SystemError::ENOMEM)?;
    data.resize(len, 0);
    reader.copy_from_user(&mut data, 0)?;
    Ok(data)
}

pub(crate) fn send_user_buffer_via_kernel_buf<S: Socket + ?Sized>(
    socket: &S,
    reader: &UserBufferReader<'_>,
    len: usize,
    flags: PMSG,
    address: Option<Endpoint>,
    kernel_buf_len: usize,
) -> Result<usize, SystemError> {
    if len == 0 {
        return if let Some(endpoint) = address {
            socket.send_to(&[], flags, endpoint)
        } else {
            socket.send(&[], flags)
        };
    }

    let scratch_len = core::cmp::min(kernel_buf_len.max(1), len);
    let mut kbuf = Vec::new();
    kbuf.try_reserve(scratch_len)
        .map_err(|_| SystemError::ENOMEM)?;
    kbuf.resize(scratch_len, 0);
    let mut total_sent = 0usize;

    while total_sent < len {
        let want = core::cmp::min(kbuf.len(), len - total_sent);
        if let Err(e) = reader.copy_from_user(&mut kbuf[..want], total_sent) {
            return if total_sent == 0 {
                Err(e)
            } else {
                Ok(total_sent)
            };
        }

        let result = if let Some(endpoint) = address.as_ref() {
            socket.send_to(&kbuf[..want], flags, endpoint.clone())
        } else {
            socket.send(&kbuf[..want], flags)
        };

        match result {
            Ok(0) => return Ok(total_sent),
            Ok(n) => {
                total_sent = total_sent.saturating_add(n);
                if n < want {
                    return Ok(total_sent);
                }
            }
            Err(e) => {
                return if total_sent == 0 {
                    Err(e)
                } else {
                    Ok(total_sent)
                }
            }
        }
    }

    Ok(total_sent)
}
