//! AF_VSOCK `SOCK_STREAM` 实现。
//!
//! 数据路径分为两类：
//! - 本地回环（`peer.cid == local_cid`）：直接写入对端 socket 的接收队列
//! - 远端 CID：委托给 transport 后端执行 connect/send/shutdown

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cmp::min;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use system_error::SystemError;

use crate::filesystem::epoll::{event_poll::EventPoll, EPollEventType};
use crate::filesystem::vfs::{fasync::FAsyncItems, iov::IoVecs, vcore::generate_inode_id, InodeId};
use crate::libs::mutex::Mutex;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::{EPollItems, ShutdownBit};
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::{RecvFromAddrBehavior, Socket, PMSG, PSO, PSOL};

use super::addr::{
    ConnectionId, VsockEndpoint, VMADDR_CID_ANY, VMADDR_CID_LOCAL, VMADDR_PORT_ANY,
};
use super::global::global_vsock_space;
use super::space::VsockSpace;
use super::transport::{
    transport_connect, transport_listen, transport_local_cid, transport_reset, transport_send,
    transport_ready, transport_shutdown, transport_unlisten, VsockTransportEvent,
};

type EP = crate::filesystem::epoll::EPollEventType;

const DEFAULT_RECV_BUF_SIZE: usize = 64 * 1024;
const DEFAULT_SEND_BUF_SIZE: usize = 64 * 1024;
const DEFAULT_BACKLOG: usize = 128;
const SOMAXCONN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VsockStreamState {
    // 初始态：尚未绑定本地端点。
    Init,
    // 已绑定本地端点（显式 bind 或 connect 自动分配）。
    Bound,
    // 监听态：可从 pending_accept 中取已完成连接。
    Listening,
    // 连接中：外连尚未完成。
    Connecting,
    // 已连接：可进行收发。
    Connected,
    // 已关闭终态。
    Closed,
}

#[derive(Debug)]
struct VsockStreamInner {
    /// 连接状态机状态（Init/Bound/Listening/Connecting/Connected/Closed）
    state: VsockStreamState,
    /// 本地端点（cid, port），bind/connect 后确定
    local: Option<VsockEndpoint>,
    /// 对端端点（cid, port），connect/accept 后确定
    peer: Option<VsockEndpoint>,
    /// 监听队列上限（listen backlog）
    backlog: usize,
    /// 已建立但尚未被 accept 取走的子连接队列
    pending_accept: VecDeque<Arc<VsockStreamSocket>>,
    /// 接收缓冲区（按字节存储）
    recv_buf: VecDeque<u8>,
    /// 本端发送方向已关闭（shutdown(SHUT_WR)）
    send_shutdown: bool,
    /// 本端接收方向已关闭（shutdown(SHUT_RD)）
    recv_shutdown: bool,
    /// 对端发送方向已关闭（本端可读到 EOF）
    peer_send_shutdown: bool,
    /// 对端连接已关闭
    peer_closed: bool,
    /// 本地回环连接时，对端 socket 的弱引用
    peer_socket: Option<Weak<VsockStreamSocket>>,
    /// 连接在 VsockSpace 中的注册键（local, peer）
    connection_id: Option<ConnectionId>,
    /// 最近一次异步 socket 错误（connect 失败、RST、transport 故障等）
    pending_error: Option<SystemError>,
    /// 远端连接的可写能力提示（用于 EPOLLOUT 与 send EAGAIN 一致性）。
    remote_writable: bool,
    /// 是否持有本地端口引用，用于释放时防止泄漏或重复释放
    port_ref_held: bool,
}

impl Default for VsockStreamInner {
    fn default() -> Self {
        Self {
            state: VsockStreamState::Init,
            local: None,
            peer: None,
            backlog: DEFAULT_BACKLOG,
            pending_accept: VecDeque::new(),
            recv_buf: VecDeque::new(),
            send_shutdown: false,
            recv_shutdown: false,
            peer_send_shutdown: false,
            peer_closed: false,
            peer_socket: None,
            connection_id: None,
            pending_error: None,
            remote_writable: true,
            port_ref_held: false,
        }
    }
}

#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct VsockStreamSocket {
    /// 可变连接状态与缓冲区数据
    inner: Mutex<VsockStreamInner>,
    /// VSOCK 地址空间（端口分配、监听/连接注册）
    space: Arc<VsockSpace>,
    /// 阻塞等待队列（accept/recv 等待条件）
    wait_queue: WaitQueue,
    /// VFS 层 inode 标识
    inode_id: InodeId,
    /// 打开该 socket 的文件引用计数
    open_files: AtomicUsize,
    /// 非阻塞模式标志
    nonblock: AtomicBool,
    /// 自身弱引用，供内部注册和回调使用
    self_ref: Weak<Self>,
    /// epoll 关注项集合
    epoll_items: EPollItems,
    /// 异步 I/O（SIGIO）订阅项集合
    fasync_items: FAsyncItems,
}

impl VsockStreamSocket {
    /// 创建一个 VSOCK 流式 socket。
    ///
    /// # 参数
    /// - `nonblock`: 初始是否设置为非阻塞模式
    ///
    /// # 返回
    /// - 新建的 `Arc<VsockStreamSocket>`
    ///
    /// # 行为
    /// - 初始化状态机为 `Init`
    /// - 绑定到全局 `VsockSpace`
    /// - 初始化 waitqueue/epoll/fasync 元数据
    pub fn new(nonblock: bool) -> Result<Arc<Self>, SystemError> {
        Ok(Arc::new_cyclic(|me| Self {
            inner: Mutex::new(VsockStreamInner::default()),
            space: global_vsock_space(),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            nonblock: AtomicBool::new(nonblock),
            self_ref: me.clone(),
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
        }))
    }

    /// 查询当前 socket 是否处于非阻塞模式。
    fn is_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    /// 获取当前本地 CID（由 transport 决定）。
    fn local_cid() -> u32 {
        transport_local_cid()
    }

    /// 规范化 bind 目标端点中的 CID。
    ///
    /// # 参数
    /// - `endpoint`: 用户传入的 bind 端点
    ///
    /// # 行为
    /// - `VMADDR_CID_ANY` / `VMADDR_CID_LOCAL` 会被转换为当前本地 CID
    /// - 若 `cid` 与本地 CID 不一致，返回 `EINVAL`
    fn normalize_bind_endpoint(endpoint: VsockEndpoint) -> Result<VsockEndpoint, SystemError> {
        let local_cid = Self::local_cid();
        let cid = match endpoint.cid {
            VMADDR_CID_ANY | VMADDR_CID_LOCAL => local_cid,
            cid if cid == local_cid => cid,
            _ => return Err(SystemError::EINVAL),
        };

        Ok(VsockEndpoint {
            cid,
            port: endpoint.port,
        })
    }

    /// 规范化 connect 目标端点中的对端 CID。
    ///
    /// # 参数
    /// - `endpoint`: 用户传入的 connect 目标端点
    ///
    /// # 行为
    /// - `VMADDR_CID_ANY` 非法，返回 `EINVAL`
    /// - `VMADDR_CID_LOCAL` 会被转换为当前本地 CID
    fn normalize_connect_peer(endpoint: VsockEndpoint) -> Result<VsockEndpoint, SystemError> {
        if endpoint.cid == VMADDR_CID_ANY {
            return Err(SystemError::EINVAL);
        }

        let cid = if endpoint.cid == VMADDR_CID_LOCAL {
            Self::local_cid()
        } else {
            endpoint.cid
        };

        Ok(VsockEndpoint {
            cid,
            port: endpoint.port,
        })
    }

    /// 获取“未绑定”语义下的本地地址占位值。
    fn unspecified_local_endpoint() -> VsockEndpoint {
        VsockEndpoint {
            cid: VMADDR_CID_ANY,
            port: 0,
        }
    }

    /// 通知 I/O 事件变化。
    ///
    /// # 行为
    /// - 唤醒阻塞在 wait queue 上的线程
    /// - 通知 epoll 重新评估就绪事件
    /// - 触发异步 I/O 的 `SIGIO`
    fn notify_io(&self) {
        // 同时覆盖阻塞等待和异步通知两类观察者。
        let _ = self.wait_queue.wake_all();

        let events = self.check_io_event();
        let _ = EventPoll::wakeup_epoll(self.epoll_items.as_ref(), events);
        self.fasync_items.send_sigio();
    }

    /// 判断当前连接是否应报告可写。
    ///
    /// 语义对齐 Linux：`EPOLLOUT` 表示“当前有发送能力”，而不是仅仅“已连接”。
    fn can_send_locked(inner: &VsockStreamInner) -> bool {
        if inner.state != VsockStreamState::Connected || inner.send_shutdown || inner.peer_closed {
            return false;
        }

        if inner.pending_error.is_some() {
            return false;
        }

        // 本地回环连接没有远端 credit 限制。
        if inner.peer_socket.is_some() {
            return true;
        }

        // 远端连接按 transport 状态 + 最近可写提示进行门控，避免可写假阳性。
        if let (Some(local), Some(peer)) = (inner.local, inner.peer) {
            if peer.cid != local.cid {
                return transport_ready() && inner.remote_writable;
            }
        }

        true
    }

    /// 更新远端连接可写提示。
    fn set_remote_writable_hint(&self, writable: bool, notify: bool) {
        let changed = {
            let mut inner = self.inner.lock();
            if inner.peer_socket.is_some() {
                return;
            }

            let changed = inner.remote_writable != writable;
            inner.remote_writable = writable;
            changed
        };

        if notify && changed {
            self.notify_io();
        }
    }

    /// 尝试执行一次接收。
    ///
    /// # 参数
    /// - `buffer`: 用户接收缓冲区
    /// - `peek`: 是否仅窥探（不消费数据）
    ///
    /// # 返回
    /// - `Ok(n)`: 成功读取 `n` 字节
    /// - `Ok(0)`: 对端正常关闭/半关闭或本端接收已关闭（EOF）
    /// - `Err(EAGAIN_OR_EWOULDBLOCK)`: 当前无数据可读
    fn try_recv_once(&self, buffer: &mut [u8], peek: bool) -> Result<usize, SystemError> {
        let mut inner = self.inner.lock();

        if inner.recv_shutdown {
            inner.recv_buf.clear();
            return Ok(0);
        }

        if !inner.recv_buf.is_empty() {
            let n = min(buffer.len(), inner.recv_buf.len());
            {
                // VecDeque 在回绕时会分成两段，分段拷贝可避免逐字节循环开销。
                let (front, back) = inner.recv_buf.as_slices();
                let front_n = min(n, front.len());
                buffer[..front_n].copy_from_slice(&front[..front_n]);

                let back_n = n - front_n;
                if back_n > 0 {
                    buffer[front_n..n].copy_from_slice(&back[..back_n]);
                }
            }

            if !peek {
                inner.recv_buf.drain(..n);
            }
            return Ok(n);
        }

        if let Some(error) = inner.pending_error.clone() {
            return Err(error);
        }

        if inner.peer_send_shutdown || inner.peer_closed || inner.state == VsockStreamState::Closed {
            return Ok(0);
        }

        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    /// 向当前 socket 的接收缓冲区入队数据。
    ///
    /// # 参数
    /// - `buffer`: 需要入队的数据
    ///
    /// # 返回
    /// - 成功返回写入字节数
    /// - 若接收方向关闭或 socket 已关闭，返回 `EPIPE`
    fn enqueue_recv_data(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        let mut inner = self.inner.lock();
        if inner.recv_shutdown || inner.state == VsockStreamState::Closed {
            return Err(SystemError::EPIPE);
        }

        inner.recv_buf.extend(buffer.iter().copied());
        drop(inner);
        self.notify_io();
        Ok(buffer.len())
    }

    /// 判断 recv 阻塞等待条件是否满足。
    fn can_recv(&self) -> bool {
        let inner = self.inner.lock();
        !inner.recv_buf.is_empty()
            || inner.peer_send_shutdown
            || inner.peer_closed
            || inner.pending_error.is_some()
    }

    /// 判断 accept 阻塞等待条件是否满足。
    fn can_accept(&self) -> bool {
        let inner = self.inner.lock();
        inner.state == VsockStreamState::Listening && !inner.pending_accept.is_empty()
    }

    /// 判断 connect 阻塞等待条件是否满足。
    fn can_complete_connect(&self) -> bool {
        let inner = self.inner.lock();
        inner.state != VsockStreamState::Connecting
    }

    /// 读取当前 connect 结果（用于阻塞等待收敛）。
    fn connect_status(&self) -> Result<(), SystemError> {
        let inner = self.inner.lock();
        match inner.state {
            VsockStreamState::Connected => Ok(()),
            VsockStreamState::Connecting => Err(SystemError::EINPROGRESS),
            VsockStreamState::Bound => Err(
                inner
                    .pending_error
                    .clone()
                    .unwrap_or(SystemError::ECONNREFUSED),
            ),
            VsockStreamState::Init | VsockStreamState::Listening => Err(SystemError::EINVAL),
            VsockStreamState::Closed => Err(SystemError::EBADF),
        }
    }

    /// 在 connect 失败后回滚到 `Bound`，并记住失败错误码。
    fn rollback_connect_to_bound(&self, error: SystemError) {
        let mut inner = self.inner.lock();
        if inner.state == VsockStreamState::Connecting {
            inner.state = VsockStreamState::Bound;
            inner.peer = None;
            inner.connection_id = None;
            inner.pending_error = Some(error);
        }
    }

    /// 确保在 connect 前已拥有本地端点。
    ///
    /// # 返回
    /// - 已绑定时直接返回当前本地端点
    /// - 未绑定时自动分配临时端口并进入 `Bound`
    ///
    /// # 行为
    /// - 对不合法状态返回对应错误（如 `EISCONN`/`EALREADY`/`EBADF`）
    fn ensure_local_bound_for_connect(&self) -> Result<VsockEndpoint, SystemError> {
        {
            let inner = self.inner.lock();
            match inner.state {
                VsockStreamState::Connected => return Err(SystemError::EISCONN),
                VsockStreamState::Connecting => return Err(SystemError::EALREADY),
                VsockStreamState::Listening => return Err(SystemError::EINVAL),
                VsockStreamState::Closed => return Err(SystemError::EBADF),
                _ => {}
            }

            if let Some(local) = inner.local {
                return Ok(local);
            }
        }

        // connect 前按 Linux 语义自动绑定一个临时端口。
        let local = VsockEndpoint {
            cid: Self::local_cid(),
            port: self.space.alloc_ephemeral_port()?,
        };

        let mut inner = self.inner.lock();
        if inner.state != VsockStreamState::Init {
            self.space.release_port(local.port);
            return Err(SystemError::EINVAL);
        }

        inner.local = Some(local);
        inner.port_ref_held = true;
        inner.state = VsockStreamState::Bound;

        Ok(local)
    }

    /// 将已建立的子连接放入监听套接字的 accept 队列。
    ///
    /// # 参数
    /// - `socket`: 已建立连接的子 socket
    ///
    /// # 行为
    /// - 仅 `Listening` 状态可入队
    /// - 超过 backlog 限制返回 `ECONNREFUSED`
    fn enqueue_accepted(&self, socket: Arc<VsockStreamSocket>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        if inner.state != VsockStreamState::Listening {
            return Err(SystemError::ECONNREFUSED);
        }

        if inner.pending_accept.len() >= inner.backlog {
            return Err(SystemError::ECONNREFUSED);
        }

        inner.pending_accept.push_back(socket);
        drop(inner);
        self.notify_io();
        Ok(())
    }

    /// 处理“对端发送方向关闭”事件。
    fn on_peer_send_shutdown(&self) {
        let mut inner = self.inner.lock();
        inner.peer_send_shutdown = true;
        drop(inner);
        self.notify_io();
    }

    /// 处理“对端连接已关闭”事件。
    fn on_peer_closed(&self) {
        let mut inner = self.inner.lock();
        inner.peer_closed = true;
        inner.peer_send_shutdown = true;
        inner.peer_socket = None;
        inner.remote_writable = false;
        drop(inner);
        self.notify_io();
    }

    /// 处理“对端复位或设备异常”事件。
    fn on_peer_reset(&self, error: SystemError) {
        let mut inner = self.inner.lock();
        inner.peer_closed = true;
        inner.peer_send_shutdown = true;
        inner.peer_socket = None;
        inner.remote_writable = false;
        inner.pending_error = Some(error);
        drop(inner);
        self.notify_io();
    }

    /// 处理 transport credit 进展事件。
    fn on_transport_credit_progress(&self) {
        self.set_remote_writable_hint(true, true);
    }

    /// 处理 transport credit 进展事件（无额外唤醒）。
    fn on_transport_credit_progress_quiet(&self) {
        self.set_remote_writable_hint(true, false);
    }

    /// 处理同 CID 的本地连接建立。
    ///
    /// # 参数
    /// - `local`: 客户端本地端点
    /// - `peer`: 服务端监听端点
    ///
    /// # 行为
    /// - 在内核内构造一对互联 socket（客户端 + accepted 子连接）
    /// - 注册 `connected` 表项与端口引用
    /// - 将子连接压入监听者的 accept 队列
    fn connect_local(&self, local: VsockEndpoint, peer: VsockEndpoint) -> Result<(), SystemError> {
        // 本地连接不经过 transport，直接在内核内完成握手。
        let listener = self.space.find_listener(peer).ok_or(SystemError::ECONNREFUSED)?;
        let client = self.self_ref.upgrade().ok_or(SystemError::ECONNRESET)?;

        let accepted = VsockStreamSocket::new(false)?;
        // accept 出来的子连接与监听者共享同一个端口，需要增加引用计数。
        self.space.retain_port(peer.port)?;

        // 注意方向性：服务端和客户端会登记两条镜像连接键。
        let server_connection_id = ConnectionId { local: peer, peer: local };
        let client_connection_id = ConnectionId { local, peer };

        {
            let mut accepted_inner = accepted.inner.lock();
            accepted_inner.state = VsockStreamState::Connected;
            accepted_inner.local = Some(peer);
            accepted_inner.peer = Some(local);
            accepted_inner.peer_socket = Some(Arc::downgrade(&client));
            accepted_inner.connection_id = Some(server_connection_id);
            accepted_inner.pending_error = None;
            accepted_inner.remote_writable = true;
            accepted_inner.port_ref_held = true;
        }

        {
            let mut client_inner = self.inner.lock();
            client_inner.state = VsockStreamState::Connected;
            client_inner.peer = Some(peer);
            client_inner.peer_socket = Some(Arc::downgrade(&accepted));
            client_inner.connection_id = Some(client_connection_id);
            client_inner.peer_closed = false;
            client_inner.peer_send_shutdown = false;
            client_inner.send_shutdown = false;
            client_inner.recv_shutdown = false;
            client_inner.pending_error = None;
            client_inner.remote_writable = true;
        }

        self.space
            .register_connected(server_connection_id, Arc::downgrade(&accepted));
        self.space
            .register_connected(client_connection_id, Arc::downgrade(&client));

        // 将子连接投递给监听者；失败时必须回滚 connected 表与端口引用。
        if let Err(error) = listener.enqueue_accepted(accepted.clone()) {
            self.space.unregister_connected(server_connection_id);
            self.space.unregister_connected(client_connection_id);
            self.space.release_port(peer.port);
            {
                let mut client_inner = self.inner.lock();
                client_inner.state = VsockStreamState::Bound;
                client_inner.peer = None;
                client_inner.peer_socket = None;
                client_inner.connection_id = None;
            }
            return Err(error);
        }

        self.notify_io();
        Ok(())
    }

    /// 处理跨 CID 的远端连接建立。
    ///
    /// # 参数
    /// - `local`: 本地端点
    /// - `peer`: 远端端点
    ///
    /// # 行为
    /// - 调用 `transport_connect` 发起远端连接请求
    /// - 连接完成由后续 `Response/Rst` 事件驱动状态迁移
    fn connect_remote(&self, local: VsockEndpoint, peer: VsockEndpoint) -> Result<(), SystemError> {
        // 远端路径完全交由 transport 后端处理。
        transport_connect(local, peer)
    }
}

impl Socket for VsockStreamSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    /// 计算当前 socket 的 epoll 就绪事件。
    ///
    /// # 行为
    /// - 监听态：仅当 accept 队列非空时报告可读
    /// - 已连接态：按接收缓存、对端关闭和本端发送状态组合出读写/挂断事件
    fn check_io_event(&self) -> EPollEventType {
        let inner = self.inner.lock();
        let mut events = EP::empty();

        // 监听 socket：accept 队列非空即视为可读（可 accept）。
        if inner.state == VsockStreamState::Listening {
            if !inner.pending_accept.is_empty() {
                events.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
            }
            return events;
        }

        if !inner.recv_buf.is_empty() || inner.peer_send_shutdown || inner.peer_closed {
            events.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
        }

        if inner.pending_error.is_some() {
            events.insert(EP::EPOLLERR);
        }

        if Self::can_send_locked(&inner) {
            events.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
        }

        if inner.peer_closed || inner.state == VsockStreamState::Closed {
            events.insert(EP::EPOLLHUP);
        }

        events
    }

    fn send_buffer_size(&self) -> usize {
        DEFAULT_SEND_BUF_SIZE
    }

    fn recv_buffer_size(&self) -> usize {
        DEFAULT_RECV_BUF_SIZE
    }

    fn recv_bytes_available(&self) -> usize {
        self.inner.lock().recv_buf.len()
    }

    fn send_bytes_available(&self) -> Result<usize, SystemError> {
        let inner = self.inner.lock();
        if Self::can_send_locked(&inner) {
            Ok(DEFAULT_SEND_BUF_SIZE)
        } else {
            Ok(0)
        }
    }

    /// 接受一个已建立连接。
    ///
    /// # 返回
    /// - `Ok((child, peer))`：返回子 socket 与对端地址
    /// - 非阻塞且暂无连接时返回 `EAGAIN_OR_EWOULDBLOCK`
    ///
    /// # 行为
    /// - 仅 `Listening` 状态允许 accept
    /// - 阻塞模式下在 wait queue 上等待直到 `can_accept()`
    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        loop {
            if let Some(child) = {
                let mut inner = self.inner.lock();
                if inner.state != VsockStreamState::Listening {
                    return Err(SystemError::EINVAL);
                }
                inner.pending_accept.pop_front()
            } {
                let peer = child.remote_endpoint()?;
                return Ok((child as Arc<dyn Socket>, peer));
            }

            if self.is_nonblock() {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            wq_wait_event_interruptible!(self.wait_queue, self.can_accept(), {})?;
        }
    }

    /// 绑定本地 vsock 地址。
    ///
    /// # 参数
    /// - `endpoint`: 仅支持 `Endpoint::Vsock`
    ///
    /// # 行为
    /// - `VMADDR_PORT_ANY` 时自动分配临时端口
    /// - 明确端口时尝试独占保留
    /// - 成功后进入 `Bound` 状态
    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let Endpoint::Vsock(endpoint) = endpoint else {
            return Err(SystemError::EAFNOSUPPORT);
        };

        let mut endpoint = Self::normalize_bind_endpoint(endpoint)?;
        let port = if endpoint.port == VMADDR_PORT_ANY {
            self.space.alloc_ephemeral_port()?
        } else {
            self.space.reserve_port(endpoint.port)?;
            endpoint.port
        };
        endpoint.port = port;

        let mut inner = self.inner.lock();
        if inner.state != VsockStreamState::Init || inner.local.is_some() {
            self.space.release_port(port);
            return Err(SystemError::EINVAL);
        }

        inner.local = Some(endpoint);
        inner.port_ref_held = true;
        inner.state = VsockStreamState::Bound;
        Ok(())
    }

    /// 关闭 socket 并执行资源回收。
    ///
    /// # 行为
    /// - 释放端口引用、注销监听/连接注册
    /// - 递归关闭 pending accept 队列中的子连接
    /// - 通知对端与本地等待者状态已变化
    fn do_close(&self) -> Result<(), SystemError> {
        // 先在锁内摘取清理所需数据，再在锁外执行回调，避免死锁风险。
        let (
            local,
            was_listener,
            peer_endpoint,
            peer_socket,
            peer_was_closed,
            close_state,
            connection_id,
            release_port,
            pending_accept,
        ) = {
            let mut inner = self.inner.lock();
            if inner.state == VsockStreamState::Closed {
                return Ok(());
            }

            let local = inner.local;
            let was_listener = inner.state == VsockStreamState::Listening;
            let peer_endpoint = inner.peer;
            let peer_socket = inner.peer_socket.take();
            let peer_was_closed = inner.peer_closed;
            let close_state = inner.state;
            let connection_id = inner.connection_id.take();
            let release_port = inner.port_ref_held;
            inner.port_ref_held = false;
            inner.state = VsockStreamState::Closed;
            inner.peer = None;
            inner.remote_writable = false;
            let pending_accept = core::mem::take(&mut inner.pending_accept);

            (
                local,
                was_listener,
                peer_endpoint,
                peer_socket,
                peer_was_closed,
                close_state,
                connection_id,
                release_port,
                pending_accept,
            )
        };

        if let Some(local) = local {
            // 先注销全局空间中的索引，再释放端口引用。
            self.space.unregister_connecting(local);
            if was_listener {
                self.space.unregister_listener(local);
                let _ = transport_unlisten(local);
            }
            if release_port {
                self.space.release_port(local.port);
            }
        }

        if let Some(connection_id) = connection_id {
            self.space.unregister_connected(connection_id);
        }

        if let (Some(local), Some(peer)) = (local, peer_endpoint) {
            if peer.cid != local.cid {
                match close_state {
                    // 连接尚未完成时主动发送 RST，避免对端半连接残留。
                    VsockStreamState::Connecting => {
                        let _ = transport_reset(local, peer);
                    }
                    // 已连接远端 close：优先 SHUTDOWN，失败时回退 RST。
                    VsockStreamState::Connected if !peer_was_closed => {
                        if let Err(error) = transport_shutdown(local, peer, true, true) {
                            if error != SystemError::ENODEV {
                                let _ = transport_reset(local, peer);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // 监听 socket 关闭时，尚未被 accept 的子连接需要一起关闭。
        for pending in pending_accept {
            let _ = pending.do_close();
        }

        // 通知本地对端连接已关闭，触发其 EOF/HUP 可见性。
        if let Some(peer) = peer_socket.and_then(|weak| weak.upgrade()) {
            peer.on_peer_closed();
        }

        self.notify_io();
        Ok(())
    }

    /// 连接到目标 vsock 端点。
    ///
    /// # 参数
    /// - `endpoint`: 仅支持 `Endpoint::Vsock`
    ///
    /// # 行为
    /// - 先确保本地端点存在（必要时自动 bind）
    /// - 同 CID 走 `connect_local`
    /// - 跨 CID 走 transport 异步握手（非阻塞返回 `EINPROGRESS`）
    /// - 失败时回滚到 `Bound`
    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let Endpoint::Vsock(endpoint) = endpoint else {
            return Err(SystemError::EAFNOSUPPORT);
        };

        if endpoint.port == VMADDR_PORT_ANY {
            return Err(SystemError::EINVAL);
        }

        let peer = Self::normalize_connect_peer(endpoint)?;
        let local = self.ensure_local_bound_for_connect()?;
        {
            let mut inner = self.inner.lock();
            if inner.state == VsockStreamState::Connected {
                return Err(SystemError::EISCONN);
            }
            if inner.state == VsockStreamState::Connecting {
                return Err(SystemError::EALREADY);
            }
            if inner.state != VsockStreamState::Bound {
                return Err(SystemError::EINVAL);
            }
            inner.state = VsockStreamState::Connecting;
            inner.peer = Some(peer);
            inner.pending_error = None;
            inner.remote_writable = true;
        }

        // 同 CID 本地直连仍是同步完成路径。
        if peer.cid == Self::local_cid() {
            let result = self.connect_local(local, peer);
            if let Err(error) = result {
                self.rollback_connect_to_bound(error.clone());
                return Err(error);
            }
            return Ok(());
        }

        // 远端连接期间在 connecting 表登记，等待 transport 事件推进状态。
        self.space.register_connecting(local, self.self_ref.clone());
        if let Err(error) = self.connect_remote(local, peer) {
            self.space.unregister_connecting(local);
            self.rollback_connect_to_bound(error.clone());
            return Err(error);
        }

        if self.is_nonblock() {
            Err(SystemError::EINPROGRESS)
        } else {
            loop {
                match self.connect_status() {
                    Err(SystemError::EINPROGRESS) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_complete_connect(), {})?;
                    }
                    result => return result,
                }
            }
        }
    }

    /// 设置阻塞/非阻塞模式。
    ///
    /// # 参数
    /// - `nonblocking`: `true` 表示非阻塞
    fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblock.store(nonblocking, Ordering::Relaxed);
    }

    /// 指定 `recvfrom` 地址输出行为。
    ///
    /// # 行为
    /// - 对 stream socket 返回 `Ignore`，符合 Linux 语义
    fn recvfrom_addr_behavior(&self) -> RecvFromAddrBehavior {
        RecvFromAddrBehavior::Ignore
    }

    /// 获取对端端点地址。
    ///
    /// # 返回
    /// - 已连接时返回 `Endpoint::Vsock(peer)`
    /// - 未连接返回 `ENOTCONN`
    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        self.inner
            .lock()
            .peer
            .map(Endpoint::Vsock)
            .ok_or(SystemError::ENOTCONN)
    }

    /// 获取本地端点地址。
    ///
    /// # 返回
    /// - 已绑定时返回真实本地端点
    /// - 未绑定时返回“未指定地址”占位值
    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let endpoint = self
            .inner
            .lock()
            .local
            .unwrap_or_else(Self::unspecified_local_endpoint);
        Ok(Endpoint::Vsock(endpoint))
    }

    /// 将当前 socket 切换到监听状态。
    ///
    /// # 参数
    /// - `backlog`: 用户传入 backlog，内部会限制到 `[1, SOMAXCONN]`
    ///
    /// # 行为
    /// - 仅 `Bound` 或 `Listening` 状态可调用
    /// - 会在 `VsockSpace` 中注册监听者
    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        let effective_backlog = backlog.max(1).min(SOMAXCONN);
        let local = {
            let inner = self.inner.lock();
            if inner.state != VsockStreamState::Bound && inner.state != VsockStreamState::Listening {
                return Err(SystemError::EINVAL);
            }
            inner.local.ok_or(SystemError::EINVAL)?
        };

        self.space
            .register_listener(local, self.self_ref.clone())?;

        let mut inner = self.inner.lock();
        if inner.state != VsockStreamState::Bound && inner.state != VsockStreamState::Listening {
            return Err(SystemError::EINVAL);
        }

        inner.backlog = effective_backlog;
        inner.state = VsockStreamState::Listening;

        // 远端连接请求由 transport 层驱动，监听端口需要同步到后端。
        if let Err(error) = transport_listen(local) {
            if error != SystemError::ENODEV {
                return Err(error);
            }
        }
        Ok(())
    }

    /// 从流式 socket 读取数据。
    ///
    /// # 参数
    /// - `buffer`: 用户接收缓冲区
    /// - `flags`: `PMSG::DONTWAIT` / `PMSG::PEEK` 等标志
    ///
    /// # 行为
    /// - 非阻塞模式下无数据立即返回 `EAGAIN_OR_EWOULDBLOCK`
    /// - 阻塞模式下等待直到 `can_recv()` 为真
    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        let nonblock = self.is_nonblock() || flags.contains(PMSG::DONTWAIT);
        let peek = flags.contains(PMSG::PEEK);

        loop {
            match self.try_recv_once(buffer, peek) {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) if !nonblock => {
                    wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                }
                result => return result,
            }
        }
    }

    /// `recvfrom` 变体，地址参数对 stream 语义仅做兼容返回。
    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        let n = self.recv(buffer, flags)?;
        let endpoint = self
            .remote_endpoint()
            .unwrap_or_else(|_| Endpoint::Vsock(Self::unspecified_local_endpoint()));
        Ok((n, endpoint))
    }

    /// `recvmsg` 实现：先聚合读取，再 scatter 回用户 iovec。
    fn recv_msg(&self, msg: &mut crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let total = iovs.total_len();
        let mut tmp = vec![0u8; total];
        let n = self.recv(&mut tmp, flags)?;
        let written = iovs.scatter(&tmp[..n])?;
        msg.msg_flags = 0;
        msg.msg_namelen = 0;
        Ok(written)
    }

    /// 向已连接对端发送数据。
    ///
    /// # 参数
    /// - `buffer`: 待发送数据
    ///
    /// # 行为
    /// - 本地回环连接优先走内核内直投路径
    /// - 远端连接走 transport 后端发送
    /// - 未连接返回 `ENOTCONN`，发送方向关闭返回 `EPIPE`
    fn send(&self, buffer: &[u8], _flags: PMSG) -> Result<usize, SystemError> {
        let (local, peer, peer_socket) = {
            let inner = self.inner.lock();
            if inner.state != VsockStreamState::Connected {
                return Err(SystemError::ENOTCONN);
            }
            if let Some(error) = inner.pending_error.clone() {
                return Err(error);
            }
            if inner.peer_closed {
                return Err(SystemError::EPIPE);
            }
            if inner.send_shutdown {
                return Err(SystemError::EPIPE);
            }
            (
                inner.local.ok_or(SystemError::ENOTCONN)?,
                inner.peer.ok_or(SystemError::ENOTCONN)?,
                inner.peer_socket.clone(),
            )
        };

        // 本地回环快路径：直接写对端 recv 缓冲区。
        if let Some(peer_socket) = peer_socket.and_then(|weak| weak.upgrade()) {
            return peer_socket.enqueue_recv_data(buffer);
        }

        // 远端路径：委托 transport。
        match transport_send(local, peer, buffer) {
            Ok(n) => {
                self.set_remote_writable_hint(true, false);
                Ok(n)
            }
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                self.set_remote_writable_hint(false, false);
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            }
            Err(SystemError::ECONNRESET) => {
                self.on_peer_reset(SystemError::ECONNRESET);
                Err(SystemError::ECONNRESET)
            }
            Err(SystemError::ENODEV) => {
                self.on_peer_reset(SystemError::ENODEV);
                Err(SystemError::ENODEV)
            }
            Err(error) => Err(error),
        }
    }

    /// `sendmsg` 实现：先 gather iovec，再复用 `send`。
    fn send_msg(&self, msg: &crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let data = iovs.gather()?;
        self.send(&data, flags)
    }

    /// `sendto` 对 stream 语义直接复用 `send`。
    fn send_to(&self, buffer: &[u8], flags: PMSG, _address: Endpoint) -> Result<usize, SystemError> {
        self.send(buffer, flags)
    }

    /// 关闭发送/接收方向（半关闭）。
    ///
    /// # 参数
    /// - `how`: 关闭方向组合（SHUT_RD / SHUT_WR / SHUT_RDWR）
    ///
    /// # 行为
    /// - 本地连接会向对端传播发送方向关闭事件
    /// - 远端连接会尝试通知 transport
    /// - 监听 socket 调用时按 Linux 语义直接成功返回
    fn shutdown(&self, how: ShutdownBit) -> Result<(), SystemError> {
        let (local, peer, peer_socket, notify_peer_send_shutdown) = {
            let mut inner = self.inner.lock();
            match inner.state {
                VsockStreamState::Connected | VsockStreamState::Closed => {}
                VsockStreamState::Listening => return Ok(()),
                _ => return Err(SystemError::ENOTCONN),
            }

            if how.is_send_shutdown() {
                inner.send_shutdown = true;
            }
            if how.is_recv_shutdown() {
                inner.recv_shutdown = true;
                inner.recv_buf.clear();
            }

            (
                inner.local,
                inner.peer,
                inner.peer_socket.clone(),
                how.is_send_shutdown(),
            )
        };

        // 本地对端：发送方向关闭需传播为对端可见 EOF 语义。
        if notify_peer_send_shutdown {
            if let Some(peer_socket) = peer_socket.and_then(|weak| weak.upgrade()) {
                peer_socket.on_peer_send_shutdown();
            }
        }

        if let (Some(local), Some(peer)) = (local, peer) {
            let _ = transport_shutdown(
                local,
                peer,
                how.is_send_shutdown(),
                how.is_recv_shutdown(),
            );
        }

        self.notify_io();
        Ok(())
    }

    /// 返回 socket inode 标识。
    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }

    /// 获取 socket 选项。
    ///
    /// # 参数
    /// - `level`: 选项层级
    /// - `name`: 选项名
    /// - `value`: 输出缓冲区
    ///
    /// # 行为
    /// - `PSOL::SOCKET/SO_ERROR` 返回并清除异步 socket 错误（Linux SO_ERROR 语义）
    /// - `PSOL::VSOCK` 当前返回 `ENOPROTOOPT`
    /// - 其他层级返回 `ENOSYS`
    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        #[inline]
        fn write_i32_opt(value: &mut [u8], v: i32) -> Result<usize, SystemError> {
            if value.len() < core::mem::size_of::<i32>() {
                return Err(SystemError::EINVAL);
            }
            value[..4].copy_from_slice(&v.to_ne_bytes());
            Ok(core::mem::size_of::<i32>())
        }

        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                match opt {
                    PSO::ERROR => {
                        // Linux SO_ERROR: 返回正 errno，并在读取后清除错误状态。
                        let mut inner = self.inner.lock();
                        let err = inner.pending_error.take().map(|e| -e.to_posix_errno()).unwrap_or(0);
                        write_i32_opt(value, err)
                    }
                    _ => Err(SystemError::ENOPROTOOPT),
                }
            }
            PSOL::VSOCK => Err(SystemError::ENOPROTOOPT),
            _ => Err(SystemError::ENOSYS),
        }
    }

    /// 设置 socket 选项。
    ///
    /// # 参数
    /// - `level`: 选项层级
    /// - `_name`: 选项名
    /// - `_val`: 输入值
    ///
    /// # 行为
    /// - `PSOL::VSOCK` 当前按“已处理”返回成功
    /// - 其他层级返回 `ENOSYS`
    fn set_option(&self, level: PSOL, _name: usize, _val: &[u8]) -> Result<(), SystemError> {
        if level == PSOL::VSOCK {
            return Ok(());
        }

        Err(SystemError::ENOSYS)
    }
}

/// 处理 transport 层上报的远端连接请求事件。
///
/// # 参数
/// - `local`: 本地监听端点
/// - `peer`: 远端发起方端点
///
/// # 行为
/// - 若无监听者则发送 RST
/// - 为请求构造子连接并压入监听 socket 的 accept 队列
/// - 资源分配失败时执行回滚并发送 RST
fn handle_transport_request_event(local: VsockEndpoint, peer: VsockEndpoint) {
    let space = global_vsock_space();
    let Some(listener) = space.find_listener(local) else {
        let _ = transport_reset(local, peer);
        return;
    };

    let child = match VsockStreamSocket::new(false) {
        Ok(socket) => socket,
        Err(error) => {
            log::warn!(
                "vsock: create child socket failed for request local={:?} peer={:?}: {:?}",
                local,
                peer,
                error
            );
            let _ = transport_reset(local, peer);
            return;
        }
    };

    if let Err(error) = space.retain_port(local.port) {
        log::warn!(
            "vsock: retain port failed for request local={:?} peer={:?}: {:?}",
            local,
            peer,
            error
        );
        let _ = transport_reset(local, peer);
        return;
    }

    let connection_id = ConnectionId { local, peer };
    {
        let mut child_inner = child.inner.lock();
        child_inner.state = VsockStreamState::Connected;
        child_inner.local = Some(local);
        child_inner.peer = Some(peer);
        child_inner.connection_id = Some(connection_id);
        child_inner.pending_error = None;
        child_inner.remote_writable = true;
        child_inner.port_ref_held = true;
    }
    space.register_connected(connection_id, Arc::downgrade(&child));

    if let Err(error) = listener.enqueue_accepted(child) {
        log::warn!(
            "vsock: enqueue accepted child failed for request local={:?} peer={:?}: {:?}",
            local,
            peer,
            error
        );
        space.unregister_connected(connection_id);
        space.release_port(local.port);
        let _ = transport_reset(local, peer);
    }
}

/// 处理 transport 层上报的连接建立响应事件。
///
/// # 参数
/// - `local`: 本地端点
/// - `peer`: 远端端点
///
/// # 行为
/// - 若命中 `connecting` 套接字，则将状态推进到 `Connected`
/// - 更新全局连接表并触发 I/O 唤醒
fn handle_transport_response_event(local: VsockEndpoint, peer: VsockEndpoint) {
    let space = global_vsock_space();
    let connection_id = ConnectionId { local, peer };
    if let Some(socket) = space.find_connected(connection_id) {
        socket.on_transport_credit_progress_quiet();
        socket.notify_io();
        return;
    }

    if let Some(socket) = space.find_connecting(local) {
        {
            let mut inner = socket.inner.lock();
            if inner.state == VsockStreamState::Connecting {
                inner.state = VsockStreamState::Connected;
                inner.peer = Some(peer);
                inner.connection_id = Some(connection_id);
                inner.peer_closed = false;
                inner.peer_send_shutdown = false;
                inner.pending_error = None;
                inner.send_shutdown = false;
                inner.recv_shutdown = false;
                inner.remote_writable = true;
            } else {
                return;
            }
        }
        space.unregister_connecting(local);
        space.register_connected(connection_id, Arc::downgrade(&socket));
        socket.notify_io();
    }
}

/// 处理 transport 层上报的数据接收事件。
///
/// # 参数
/// - `local`: 本地端点
/// - `peer`: 远端端点
/// - `data`: 收到的数据负载
///
/// # 行为
/// - 将数据写入对应连接的接收缓冲区
fn handle_transport_data_event(local: VsockEndpoint, peer: VsockEndpoint, data: Vec<u8>) {
    let space = global_vsock_space();
    let connection_id = ConnectionId { local, peer };
    if let Some(socket) = space.find_connected(connection_id) {
        socket.on_transport_credit_progress_quiet();
        let _ = socket.enqueue_recv_data(&data);
    }
}

/// 处理 transport 层上报的 SHUTDOWN 事件。
///
/// # 参数
/// - `local`: 本地端点
/// - `peer`: 远端端点
/// - `send_shutdown`: 对端发送方向关闭
/// - `recv_shutdown`: 对端接收方向关闭
///
/// # 行为
/// - 将半关闭/关闭状态同步到对应 socket
fn handle_transport_shutdown_event(
    local: VsockEndpoint,
    peer: VsockEndpoint,
    send_shutdown: bool,
    recv_shutdown: bool,
) {
    let space = global_vsock_space();
    let connection_id = ConnectionId { local, peer };
    if let Some(socket) = space.find_connected(connection_id) {
        if send_shutdown {
            socket.on_peer_send_shutdown();
        }
        if recv_shutdown {
            socket.on_peer_closed();
        }
    }
}

/// 处理 transport 层上报的 RST 事件。
///
/// # 参数
/// - `local`: 本地端点
/// - `peer`: 远端端点
///
/// # 行为
/// - 若为已连接套接字，标记对端关闭
/// - 若为连接中套接字，回滚到 `Bound`
fn handle_transport_reset_event(local: VsockEndpoint, peer: VsockEndpoint) {
    let space = global_vsock_space();
    let connection_id = ConnectionId { local, peer };
    if let Some(socket) = space.find_connected(connection_id) {
        space.unregister_connected(connection_id);
        socket.on_peer_reset(SystemError::ECONNRESET);
        return;
    }

    if let Some(socket) = space.find_connecting(local) {
        {
            let mut inner = socket.inner.lock();
            if inner.state == VsockStreamState::Connecting {
                inner.state = VsockStreamState::Bound;
                inner.peer = None;
                inner.connection_id = None;
                inner.pending_error = Some(SystemError::ECONNRESET);
            }
        }
        space.unregister_connecting(local);
        socket.notify_io();
    }
}

fn handle_transport_credit_event(local: VsockEndpoint, peer: VsockEndpoint) {
    let space = global_vsock_space();
    let connection_id = ConnectionId { local, peer };
    if let Some(socket) = space.find_connected(connection_id) {
        socket.on_transport_credit_progress();
    }
}

/// 分发 transport 事件到 `VsockStreamSocket` 语义层。
///
/// # 参数
/// - `events`: 一批 transport 上报事件
///
/// # 行为
/// - 按事件类型调用对应处理器
/// - `CreditRequest/CreditUpdate` 会推进发送可写状态并唤醒等待者
pub(super) fn dispatch_transport_events(events: Vec<VsockTransportEvent>) {
    for event in events {
        match event {
            VsockTransportEvent::Request { local, peer } => {
                handle_transport_request_event(local, peer);
            }
            VsockTransportEvent::Response { local, peer } => {
                handle_transport_response_event(local, peer);
            }
            VsockTransportEvent::Rw { local, peer, data } => {
                handle_transport_data_event(local, peer, data);
            }
            VsockTransportEvent::Shutdown {
                local,
                peer,
                send_shutdown,
                recv_shutdown,
            } => {
                handle_transport_shutdown_event(local, peer, send_shutdown, recv_shutdown);
            }
            VsockTransportEvent::Rst { local, peer } => {
                handle_transport_reset_event(local, peer);
            }
            VsockTransportEvent::CreditUpdate { local, peer }
            | VsockTransportEvent::CreditRequest { local, peer } => {
                // 对端 credit 变化可能使 send 从 EAGAIN 恢复为可写，需要唤醒 poll/epoll。
                handle_transport_credit_event(local, peer);
            }
        }
    }
}

/// 处理 transport worker 的全局错误。
///
/// # 参数
/// - `error`: 需要传播到连接的错误码
///
/// # 行为
/// - 将所有 connecting 连接回滚到 `Bound` 并记录错误
/// - 将所有 connected 连接标记为关闭并记录错误
/// - 唤醒所有受影响 socket，避免阻塞调用卡死
pub(super) fn handle_transport_fatal_error(error: SystemError) {
    let space = global_vsock_space();

    for (_local, socket) in space.take_all_connecting() {
        {
            let mut inner = socket.inner.lock();
            if inner.state == VsockStreamState::Connecting {
                inner.state = VsockStreamState::Bound;
                inner.peer = None;
                inner.connection_id = None;
                inner.pending_error = Some(error.clone());
                inner.remote_writable = false;
            }
        }
        socket.notify_io();
    }

    for (id, socket) in space.take_all_connected() {
        // 同 CID 本地回环连接不依赖 transport，不应被 worker 错误波及。
        if id.peer.cid == id.local.cid {
            space.register_connected(id, Arc::downgrade(&socket));
            continue;
        }

        {
            let mut inner = socket.inner.lock();
            if inner.state == VsockStreamState::Connected {
                inner.peer_closed = true;
                inner.peer_send_shutdown = true;
                inner.peer_socket = None;
                inner.pending_error = Some(error.clone());
                inner.remote_writable = false;
            }
        }
        socket.notify_io();
    }
}
