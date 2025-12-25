use crate::{
    filesystem::vfs::iov::IoVecs,
    filesystem::vfs::{
        fasync::FAsyncItems, utils::DName, vcore::generate_inode_id, FilePrivateData, InodeId,
    },
    libs::rwlock::RwLock,
    libs::spinlock::SpinLock,
    libs::wait_queue::WaitQueue,
    net::{
        posix::MsgHdr,
        socket::{
            self,
            common::EPollItems,
            endpoint::Endpoint,
            unix::{UnixEndpoint, UnixEndpointBound},
            Socket, PMSG, PSOL,
        },
    },
};
use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use hashbrown::HashMap;
use system_error::SystemError;

use super::ns;

use crate::libs::wait_queue::{TimeoutWaker, Waiter};
use crate::time::timer::{next_n_us_timer_jiffies, Timer};
use crate::time::{Duration, Instant};

use crate::syscall::user_access::UserBufferWriter;

// Socket ioctls used by gVisor unix socket tests.
const TIOCOUTQ: u32 = 0x5411; // Get output queue size
const FIONREAD: u32 = 0x541B; // Get input queue size (aka TIOCINQ)
const SIOCGIFINDEX: u32 = 0x8933; // name -> if_index mapping

fn clamp_usize_to_i32(v: usize) -> i32 {
    core::cmp::min(v, i32::MAX as usize) as i32
}

/// Unix 域数据报消息
#[derive(Debug, Clone)]
struct DatagramMessage {
    /// 消息数据
    data: Vec<u8>,
    /// 发送方地址
    sender_addr: Option<UnixEndpointBound>,
    /// 发送端 SO_SNDBUF 记账的长度
    sender_accounted_len: usize,
}

impl DatagramMessage {
    fn new(data: Vec<u8>, sender_addr: Option<UnixEndpointBound>) -> Self {
        let sender_accounted_len = data.len();
        Self {
            data,
            sender_addr,
            sender_accounted_len,
        }
    }
}

/// Unix 域数据报 Socket 的内部状态
#[derive(Debug)]
struct Inner {
    /// 本地绑定地址
    local_addr: Option<UnixEndpointBound>,
    /// 连接的对端地址（用于 connect 后的 send）
    peer_addr: Option<UnixEndpointBound>,
    /// 接收队列 - 保存接收到的数据报
    recv_queue: VecDeque<DatagramMessage>,
    /// 接收队列的最大容量（消息数量）
    recv_queue_capacity: usize,
}

impl Inner {
    const DEFAULT_RECV_QUEUE_CAPACITY: usize = 4096;

    fn new() -> Self {
        Self {
            local_addr: None,
            peer_addr: None,
            recv_queue: VecDeque::new(),
            recv_queue_capacity: Self::DEFAULT_RECV_QUEUE_CAPACITY,
        }
    }

    fn bind(&mut self, endpoint: UnixEndpoint) -> Result<(), SystemError> {
        if self.local_addr.is_some() {
            return endpoint.bind_unnamed();
        }
        let bound_addr = endpoint.bind()?;
        self.local_addr = Some(bound_addr);
        Ok(())
    }

    fn connect(&mut self, endpoint: UnixEndpoint) -> Result<(), SystemError> {
        let peer_addr = endpoint.connect()?;
        self.peer_addr = Some(peer_addr);
        Ok(())
    }

    fn local_endpoint(&self) -> Option<UnixEndpointBound> {
        self.local_addr.clone()
    }

    fn peer_endpoint(&self) -> Option<UnixEndpointBound> {
        self.peer_addr.clone()
    }

    fn push_message(&mut self, msg: DatagramMessage) -> Result<(), SystemError> {
        if self.recv_queue.len() >= self.recv_queue_capacity {
            return Err(SystemError::ENOBUFS);
        }
        self.recv_queue.push_back(msg);
        Ok(())
    }

    fn pop_message(&mut self) -> Option<DatagramMessage> {
        self.recv_queue.pop_front()
    }

    fn has_message(&self) -> bool {
        !self.recv_queue.is_empty()
    }
}

/// Unix 域数据报 Socket 的绑定表
/// 用于根据地址查找对应的 socket
struct BindTable {
    /// 路径绑定的 socket
    path_sockets: RwLock<HashMap<DName, Weak<UnixDatagramSocket>>>,
    /// 抽象名称绑定的 socket
    abstract_sockets: RwLock<HashMap<Arc<[u8]>, Weak<UnixDatagramSocket>>>,
}

impl BindTable {
    fn new() -> Self {
        Self {
            path_sockets: RwLock::new(HashMap::new()),
            abstract_sockets: RwLock::new(HashMap::new()),
        }
    }

    fn register(&self, addr: &UnixEndpointBound, socket: &Arc<UnixDatagramSocket>) {
        match addr {
            UnixEndpointBound::Path(path) => {
                self.path_sockets
                    .write()
                    .insert(path.clone(), Arc::downgrade(socket));
            }
            UnixEndpointBound::Abstract(handle) => {
                self.abstract_sockets
                    .write()
                    .insert(handle.name(), Arc::downgrade(socket));
            }
        }
    }

    fn unregister(&self, addr: &UnixEndpointBound) {
        match addr {
            UnixEndpointBound::Path(path) => {
                self.path_sockets.write().remove(path);
            }
            UnixEndpointBound::Abstract(handle) => {
                self.abstract_sockets.write().remove(&handle.name());
            }
        }
    }

    fn lookup(&self, addr: &UnixEndpointBound) -> Option<Arc<UnixDatagramSocket>> {
        match addr {
            UnixEndpointBound::Path(path) => {
                self.path_sockets.read().get(path).and_then(Weak::upgrade)
            }
            UnixEndpointBound::Abstract(handle) => self
                .abstract_sockets
                .read()
                .get(&handle.name())
                .and_then(Weak::upgrade),
        }
    }
}

lazy_static! {
    static ref BIND_TABLE: BindTable = BindTable::new();
}

/// Unix 域数据报 Socket
///
/// 实现无连接的、可靠的、保持消息边界的数据报传输。
/// 与 UDP 不同，Unix 域数据报在本地是可靠的（不会丢包）。
#[derive(Debug)]
#[cast_to([sync] Socket)]
pub struct UnixDatagramSocket {
    inner: SpinLock<Inner>,
    epitems: EPollItems,
    fasync_items: FAsyncItems,
    wait_queue: Arc<WaitQueue>,
    inode_id: InodeId,
    is_nonblocking: AtomicBool,
    sndbuf: AtomicUsize,
    snd_used: AtomicUsize,
    rcvbuf: AtomicUsize,
    send_timeout_us: AtomicU64,
    recv_timeout_us: AtomicU64,
    self_weak: Weak<UnixDatagramSocket>,

    is_read_shutdown: AtomicBool,
    is_write_shutdown: AtomicBool,
}

impl UnixDatagramSocket {
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;
    pub const MIN_SOCKET_BUF_SIZE: usize = 1024;
    /// 单个消息的最大大小
    // Linux unix-dgram does not impose a tiny 64KiB limit; large datagrams are allowed
    // and failures are generally reported as ENOBUFS when memory/socket buffers are insufficient.
    // gVisor tests require at least 4MiB.
    pub const MAX_MSG_SIZE: usize = 16 * 1024 * 1024;

    pub fn new(is_nonblocking: bool) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            inner: SpinLock::new(Inner::new()),
            epitems: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            wait_queue: Arc::new(WaitQueue::default()),
            inode_id: generate_inode_id(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            sndbuf: AtomicUsize::new(Self::DEFAULT_BUF_SIZE),
            snd_used: AtomicUsize::new(0),
            rcvbuf: AtomicUsize::new(Self::DEFAULT_BUF_SIZE),
            send_timeout_us: AtomicU64::new(0),
            recv_timeout_us: AtomicU64::new(0),
            self_weak: weak.clone(),

            is_read_shutdown: AtomicBool::new(false),
            is_write_shutdown: AtomicBool::new(false),
        })
    }

    /// 创建一对已连接的 Unix 数据报 socket
    pub fn new_pair(is_nonblocking: bool) -> (Arc<Self>, Arc<Self>) {
        let socket_a = Self::new(is_nonblocking);
        let socket_b = Self::new(is_nonblocking);

        // 为每个 socket 分配一个临时的抽象地址
        let addr_a = ns::alloc_ephemeral_abstract_name().ok();
        let addr_b = ns::alloc_ephemeral_abstract_name().ok();

        // 设置本地和对端地址
        {
            let mut inner_a = socket_a.inner.lock();
            inner_a.local_addr = addr_a.clone().map(UnixEndpointBound::Abstract);
            inner_a.peer_addr = addr_b.clone().map(UnixEndpointBound::Abstract);
        }
        {
            let mut inner_b = socket_b.inner.lock();
            inner_b.local_addr = addr_b.clone().map(UnixEndpointBound::Abstract);
            inner_b.peer_addr = addr_a.clone().map(UnixEndpointBound::Abstract);
        }

        // 注册到绑定表
        if let Some(ref addr) = addr_a {
            BIND_TABLE.register(&UnixEndpointBound::Abstract(addr.clone()), &socket_a);
        }
        if let Some(ref addr) = addr_b {
            BIND_TABLE.register(&UnixEndpointBound::Abstract(addr.clone()), &socket_b);
        }

        (socket_a, socket_b)
    }

    pub fn ioctl_fionread(&self) -> usize {
        let inner = self.inner.lock();
        inner.recv_queue.front().map(|m| m.data.len()).unwrap_or(0)
    }

    pub fn ioctl_tiocoutq(&self) -> usize {
        // For datagram sockets, TIOCOUTQ is not relied upon by our current tests.
        // Returning 0 is consistent with an empty send queue in the common case.
        0
    }

    fn is_nonblocking(&self) -> bool {
        self.is_nonblocking.load(Ordering::Relaxed)
    }

    fn send_timeout(&self) -> Option<Duration> {
        let us = self.send_timeout_us.load(Ordering::Relaxed);
        if us == 0 {
            None
        } else {
            Some(Duration::from_micros(us))
        }
    }

    fn recv_timeout(&self) -> Option<Duration> {
        let us = self.recv_timeout_us.load(Ordering::Relaxed);
        if us == 0 {
            None
        } else {
            Some(Duration::from_micros(us))
        }
    }

    fn parse_u32_opt(optval: &[u8]) -> Result<u32, SystemError> {
        if optval.len() < core::mem::size_of::<u32>() {
            return Err(SystemError::EINVAL);
        }
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&optval[..4]);
        Ok(u32::from_ne_bytes(raw))
    }

    fn parse_timeval_opt(optval: &[u8]) -> Result<Duration, SystemError> {
        // Linux struct timeval: { long tv_sec; long tv_usec; }
        // 兼容 32/64 位长度差异：优先按 8+8 解析，其次按 8+4。
        if optval.len() >= 16 {
            let mut sec_raw = [0u8; 8];
            let mut usec_raw = [0u8; 8];
            sec_raw.copy_from_slice(&optval[..8]);
            usec_raw.copy_from_slice(&optval[8..16]);
            let sec = i64::from_ne_bytes(sec_raw);
            let usec = i64::from_ne_bytes(usec_raw);
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(SystemError::EINVAL);
            }
            let total_us = (sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(usec as u64);
            return Ok(Duration::from_micros(total_us));
        }

        if optval.len() >= 12 {
            let mut sec_raw = [0u8; 8];
            let mut usec_raw = [0u8; 4];
            sec_raw.copy_from_slice(&optval[..8]);
            usec_raw.copy_from_slice(&optval[8..12]);
            let sec = i64::from_ne_bytes(sec_raw);
            let usec = i32::from_ne_bytes(usec_raw) as i64;
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(SystemError::EINVAL);
            }
            let total_us = (sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(usec as u64);
            return Ok(Duration::from_micros(total_us));
        }

        Err(SystemError::EINVAL)
    }

    fn write_timeval(value: &mut [u8], us: u64) -> Result<usize, SystemError> {
        if value.len() < 16 {
            return Err(SystemError::EINVAL);
        }
        let sec = (us / 1_000_000) as i64;
        let usec = (us % 1_000_000) as i64;
        value[..8].copy_from_slice(&sec.to_ne_bytes());
        value[8..16].copy_from_slice(&usec.to_ne_bytes());
        Ok(16)
    }

    fn effective_sockbuf(requested: usize) -> usize {
        // Linux sk_{snd,rcv}buf 通常会把用户设置值放大（常见为 2x）用于 bookkeeping。
        let requested = core::cmp::max(Self::MIN_SOCKET_BUF_SIZE, requested);
        requested.saturating_mul(2)
    }

    fn wait_event_interruptible_timeout<F>(
        &self,
        mut cond: F,
        timeout: Option<Duration>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
    {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if cond() {
                return Ok(());
            }

            // 检查超时
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
            }

            let remain =
                deadline.map(|d| d.duration_since(Instant::now()).unwrap_or(Duration::ZERO));

            let (waiter, waker) = Waiter::new_pair();
            self.wait_queue.register_waker(waker.clone())?;

            // 条件可能在入队后立即满足
            if cond() {
                self.wait_queue.remove_waker(&waker);
                return Ok(());
            }

            // 可中断等待：检查信号
            if crate::arch::ipc::signal::Signal::signal_pending_state(
                true,
                false,
                &crate::process::ProcessManager::current_pcb(),
            ) {
                self.wait_queue.remove_waker(&waker);
                return Err(SystemError::ERESTARTSYS);
            }

            // 如果有超时，设置定时器
            let timer = if let Some(remain) = remain {
                if remain == Duration::ZERO {
                    self.wait_queue.remove_waker(&waker);
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                let sleep_us = remain.total_micros();
                let t: Arc<Timer> = Timer::new(
                    TimeoutWaker::new(waker.clone()),
                    next_n_us_timer_jiffies(sleep_us),
                );
                t.activate();
                Some(t)
            } else {
                None
            };

            let wait_res = waiter.wait(true);
            let was_timeout = timer.as_ref().map(|t| t.timeout()).unwrap_or(false);
            if !was_timeout {
                if let Some(t) = timer {
                    t.cancel();
                }
            }

            self.wait_queue.remove_waker(&waker);

            if let Err(SystemError::ERESTARTSYS) = wait_res {
                return Err(SystemError::ERESTARTSYS);
            }
            wait_res?;
        }
    }

    fn send_buffer_available(&self, len: usize) -> bool {
        let sndbuf = self.sndbuf.load(Ordering::Relaxed);
        let used = self.snd_used.load(Ordering::Relaxed);
        used.saturating_add(len) <= sndbuf
    }

    fn try_account_send_buffer(&self, len: usize) -> Result<(), SystemError> {
        loop {
            let sndbuf = self.sndbuf.load(Ordering::Relaxed);
            // For datagram sockets, the whole message must fit in the send buffer.
            // If the message itself is larger than SO_SNDBUF, Linux returns EMSGSIZE.
            if len > sndbuf {
                return Err(SystemError::EMSGSIZE);
            }
            let used = self.snd_used.load(Ordering::Relaxed);
            if used.saturating_add(len) > sndbuf {
                return Err(SystemError::ENOBUFS);
            }
            if self
                .snd_used
                .compare_exchange(used, used + len, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn unaccount_send_buffer(&self, len: usize) {
        self.snd_used.fetch_sub(len, Ordering::SeqCst);
        self.wait_queue
            .wakeup(Some(crate::process::ProcessState::Blocked(true)));
    }

    fn release_sender_accounting(sender_addr: &Option<UnixEndpointBound>, accounted_len: usize) {
        if let Some(ref addr) = sender_addr {
            if let Some(sender_socket) = BIND_TABLE.lookup(addr) {
                sender_socket.unaccount_send_buffer(accounted_len);
            }
        }
    }

    /// 发送数据报到指定地址
    fn try_send_to(
        &self,
        buffer: &[u8],
        target_addr: &UnixEndpointBound,
    ) -> Result<usize, SystemError> {
        if self.is_write_shutdown.load(Ordering::Relaxed) {
            return Err(SystemError::EPIPE);
        }

        if buffer.len() > Self::MAX_MSG_SIZE {
            return Err(SystemError::EMSGSIZE);
        }

        // 查找目标 socket
        let target_socket = BIND_TABLE
            .lookup(target_addr)
            .ok_or(SystemError::ECONNREFUSED)?;

        if target_socket.is_read_shutdown.load(Ordering::Relaxed) {
            return Err(SystemError::EPIPE);
        }

        // 获取发送方地址
        let sender_addr = self.inner.lock().local_endpoint();

        // 发送侧 SO_SNDBUF 记账。gVisor 用例依赖：缓冲区满时非阻塞 send 返回 EWOULDBLOCK，
        // 增大 SO_SNDBUF 后可继续 send。
        self.try_account_send_buffer(buffer.len())?;

        // 创建消息
        let msg = DatagramMessage::new(buffer.to_vec(), sender_addr);

        // 将消息放入目标 socket 的接收队列
        {
            let mut target_inner = target_socket.inner.lock();
            if let Err(e) = target_inner.push_message(msg) {
                // 回滚记账并返回错误
                self.unaccount_send_buffer(buffer.len());
                return Err(e);
            }
        }

        // 唤醒目标 socket 的等待队列
        target_socket
            .wait_queue
            .wakeup(Some(crate::process::ProcessState::Blocked(true)));

        // 发送 fasync 信号
        target_socket.fasync_items.send_sigio();

        Ok(buffer.len())
    }

    /// 从接收队列接收数据报
    ///
    /// 此函数尝试从接收队列中取出一条数据报，并将其数据复制到提供的缓冲区中。
    /// 支持通过 `peek` 参数实现 MSG_PEEK 语义（查看但不消费消息）。
    ///
    /// # 参数
    ///
    /// * `buffer` - 用于接收数据的输出缓冲区
    /// * `peek` - 是否启用 PEEK 模式。为 `true` 时仅查看消息而不从队列中移除，
    ///   也不会释放发送端的 SO_SNDBUF 记账
    ///
    /// # 返回值
    ///
    /// 成功时返回 `Ok((copy_len, sender_addr, orig_len))`，其中：
    ///
    /// * `copy_len` - 实际复制到 `buffer` 中的字节数。若缓冲区大小不足，
    ///   此值会小于消息原始长度（数据报会被静默截断）
    /// * `sender_addr` - 发送方的绑定地址。如果发送方未绑定地址则为 `None`
    /// * `orig_len` - 消息的原始长度（未截断前的字节数），可用于判断
    ///   是否发生截断以及设置 MSG_TRUNC 标志
    ///
    /// # 错误
    ///
    /// * `SystemError::EAGAIN_OR_EWOULDBLOCK` - 接收队列为空
    fn try_recv_from(
        &self,
        buffer: &mut [u8],
        peek: bool,
    ) -> Result<(usize, Option<UnixEndpointBound>, usize), SystemError> {
        let mut inner = self.inner.lock();

        if peek {
            let msg = inner
                .recv_queue
                .front()
                .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

            let orig_len = msg.data.len();

            let copy_len = core::cmp::min(buffer.len(), orig_len);
            buffer[..copy_len].copy_from_slice(&msg.data[..copy_len]);

            // MSG_PEEK must not consume the message and must not release sender accounting.
            return Ok((copy_len, msg.sender_addr.clone(), orig_len));
        }

        let msg = inner
            .pop_message()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

        let orig_len = msg.data.len();

        let copy_len = core::cmp::min(buffer.len(), orig_len);
        buffer[..copy_len].copy_from_slice(&msg.data[..copy_len]);

        // 释放发送端 SO_SNDBUF 记账并唤醒等待发送者。
        Self::release_sender_accounting(&msg.sender_addr, msg.sender_accounted_len);

        // 如果缓冲区太小，数据会被截断（SOCK_DGRAM 行为）
        Ok((copy_len, msg.sender_addr, orig_len))
    }

    fn recv_return_len(copy_len: usize, orig_len: usize, flags: PMSG) -> usize {
        if flags.contains(PMSG::TRUNC) {
            orig_len
        } else {
            copy_len
        }
    }

    fn convert_enobufs_to_eagain(result: Result<usize, SystemError>) -> Result<usize, SystemError> {
        match result {
            Ok(len) => Ok(len),
            Err(SystemError::ENOBUFS) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Err(e) => Err(e),
        }
    }

    fn can_recv(&self) -> bool {
        self.inner.lock().has_message()
    }
}

impl Socket for UnixDatagramSocket {
    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        if arg == 0 {
            return Err(SystemError::EFAULT);
        }

        match cmd {
            FIONREAD => {
                let available = self.ioctl_fionread();
                let mut writer =
                    UserBufferWriter::new(arg as *mut u8, core::mem::size_of::<i32>(), true)?;
                writer
                    .buffer_protected(0)?
                    .write_one::<i32>(0, &clamp_usize_to_i32(available))?;
                Ok(0)
            }
            TIOCOUTQ => {
                let queued = self.ioctl_tiocoutq();
                let mut writer =
                    UserBufferWriter::new(arg as *mut u8, core::mem::size_of::<i32>(), true)?;
                writer
                    .buffer_protected(0)?
                    .write_one::<i32>(0, &clamp_usize_to_i32(queued))?;
                Ok(0)
            }
            SIOCGIFINDEX => Err(SystemError::ENODEV),
            _ => Err(SystemError::ENOSYS),
        }
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let unix_endpoint = UnixEndpoint::try_from(endpoint)?;
        self.inner.lock().connect(unix_endpoint)
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let unix_endpoint = UnixEndpoint::try_from(endpoint)?;

        // 先绑定地址
        let (bound_addr, should_register) = {
            let mut inner = self.inner.lock();
            let was_unbound = inner.local_addr.is_none();
            inner.bind(unix_endpoint)?;
            (inner.local_addr.clone(), was_unbound)
        };

        // 注册到绑定表（filesystem / abstract）。
        // 使用创建时保存的 Weak<Self> 来获取 Arc<Self>。
        if should_register {
            if let Some(addr) = bound_addr {
                if let Some(this) = self.self_weak.upgrade() {
                    BIND_TABLE.register(&addr, &this);
                }
            }
        }

        Ok(())
    }

    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        // 数据报 socket 不支持 listen
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        // 数据报 socket 不支持 accept
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn set_option(&self, level: PSOL, optname: usize, optval: &[u8]) -> Result<(), SystemError> {
        if !matches!(level, PSOL::SOCKET) {
            return Err(SystemError::ENOPROTOOPT);
        }

        let opt = crate::net::socket::PSO::try_from(optname as u32)
            .map_err(|_| SystemError::ENOPROTOOPT)?;

        match opt {
            crate::net::socket::PSO::SNDBUF | crate::net::socket::PSO::SNDBUFFORCE => {
                let requested = Self::parse_u32_opt(optval)? as usize;
                let new_size = Self::effective_sockbuf(requested);
                self.sndbuf.store(new_size, Ordering::SeqCst);
                self.wait_queue
                    .wakeup(Some(crate::process::ProcessState::Blocked(true)));
                Ok(())
            }
            crate::net::socket::PSO::RCVBUF | crate::net::socket::PSO::RCVBUFFORCE => {
                let requested = Self::parse_u32_opt(optval)? as usize;
                let new_size = Self::effective_sockbuf(requested);
                self.rcvbuf.store(new_size, Ordering::SeqCst);
                Ok(())
            }
            crate::net::socket::PSO::SNDTIMEO_OLD | crate::net::socket::PSO::SNDTIMEO_NEW => {
                let d = Self::parse_timeval_opt(optval)?;
                self.send_timeout_us
                    .store(d.total_micros(), Ordering::SeqCst);
                Ok(())
            }
            crate::net::socket::PSO::RCVTIMEO_OLD | crate::net::socket::PSO::RCVTIMEO_NEW => {
                let d = Self::parse_timeval_opt(optval)?;
                self.recv_timeout_us
                    .store(d.total_micros(), Ordering::SeqCst);
                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let inner = self.inner.lock();
        let addr = inner
            .local_endpoint()
            .map(|a| Endpoint::Unix(a.into()))
            .unwrap_or(Endpoint::Unix(UnixEndpoint::Unnamed));
        Ok(addr)
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        let inner = self.inner.lock();
        inner
            .peer_endpoint()
            .map(|a| Endpoint::Unix(a.into()))
            .ok_or(SystemError::ENOTCONN)
    }

    fn recv(&self, buffer: &mut [u8], flags: socket::PMSG) -> Result<usize, SystemError> {
        let peek = flags.contains(PMSG::PEEK);
        let nonblock = self.is_nonblocking() || flags.contains(PMSG::DONTWAIT);

        if nonblock {
            return self
                .try_recv_from(buffer, peek)
                .map(|(copy_len, _addr, orig_len)| {
                    Self::recv_return_len(copy_len, orig_len, flags)
                });
        }

        loop {
            match self.try_recv_from(buffer, peek) {
                Ok((copy_len, _addr, orig_len)) => {
                    return Ok(Self::recv_return_len(copy_len, orig_len, flags));
                }
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    self.wait_event_interruptible_timeout(|| self.can_recv(), self.recv_timeout())?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: socket::PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        let peek = flags.contains(PMSG::PEEK);
        let nonblock = self.is_nonblocking() || flags.contains(PMSG::DONTWAIT);

        let do_recv = |buffer: &mut [u8]| -> Result<(usize, Endpoint), SystemError> {
            let (copy_len, addr, orig_len) = self.try_recv_from(buffer, peek)?;
            let endpoint = addr
                .map(|a| Endpoint::Unix(a.into()))
                .unwrap_or(Endpoint::Unix(UnixEndpoint::Unnamed));
            let ret_len = Self::recv_return_len(copy_len, orig_len, flags);
            Ok((ret_len, endpoint))
        };

        if nonblock {
            return do_recv(buffer);
        }

        loop {
            match do_recv(buffer) {
                Ok(result) => return Ok(result),
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    self.wait_event_interruptible_timeout(|| self.can_recv(), self.recv_timeout())?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn recv_msg(&self, msg: &mut MsgHdr, flags: socket::PMSG) -> Result<usize, SystemError> {
        // recvmsg 最小语义：
        // - 不产生控制消息：将 msg_controllen 写回 0，避免用户态 CMSG_FIRSTHDR 非空
        // - 若数据报被截断：设置 MSG_TRUNC
        // - 若需要：填写 msg_name/msg_namelen

        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let mut buf = iovs.new_buf(true);
        let buf_cap = buf.len();

        let nonblock = self.is_nonblocking() || flags.contains(PMSG::DONTWAIT);
        let peek = flags.contains(PMSG::PEEK);

        let (copy_len, sender_addr, orig_len) = if nonblock {
            let mut inner = self.inner.lock();

            if peek {
                let msg_in = inner
                    .recv_queue
                    .front()
                    .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

                let orig_len = msg_in.data.len();
                let copy_len = core::cmp::min(buf_cap, orig_len);
                buf[..copy_len].copy_from_slice(&msg_in.data[..copy_len]);

                (copy_len, msg_in.sender_addr.clone(), orig_len)
            } else {
                let msg_in = inner
                    .pop_message()
                    .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

                let orig_len = msg_in.data.len();
                let copy_len = core::cmp::min(buf_cap, orig_len);
                buf[..copy_len].copy_from_slice(&msg_in.data[..copy_len]);

                Self::release_sender_accounting(&msg_in.sender_addr, msg_in.sender_accounted_len);

                (copy_len, msg_in.sender_addr, orig_len)
            }
        } else {
            loop {
                if peek {
                    let got = {
                        let inner = self.inner.lock();
                        inner.recv_queue.front().map(|msg_in| {
                            let orig_len = msg_in.data.len();
                            let copy_len = core::cmp::min(buf_cap, orig_len);
                            buf[..copy_len].copy_from_slice(&msg_in.data[..copy_len]);
                            (copy_len, msg_in.sender_addr.clone(), orig_len)
                        })
                    };
                    if let Some(v) = got {
                        break v;
                    }
                } else {
                    let popped = {
                        let mut inner = self.inner.lock();
                        inner.pop_message()
                    };
                    if let Some(msg_in) = popped {
                        let orig_len = msg_in.data.len();
                        let copy_len = core::cmp::min(buf_cap, orig_len);
                        buf[..copy_len].copy_from_slice(&msg_in.data[..copy_len]);

                        Self::release_sender_accounting(
                            &msg_in.sender_addr,
                            msg_in.sender_accounted_len,
                        );

                        break (copy_len, msg_in.sender_addr, orig_len);
                    }
                }

                self.wait_event_interruptible_timeout(|| self.can_recv(), self.recv_timeout())?;
            }
        };

        iovs.scatter(&buf[..copy_len])?;

        // 写回来源地址
        let endpoint = sender_addr
            .map(|a| Endpoint::Unix(a.into()))
            .unwrap_or(Endpoint::Unix(UnixEndpoint::Unnamed));
        endpoint.write_to_user(msg.msg_name, &mut msg.msg_namelen as *mut u32)?;

        // 不产生控制消息
        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        if orig_len > buf_cap {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }

        Ok(Self::recv_return_len(copy_len, orig_len, flags))
    }

    fn send(&self, buffer: &[u8], flags: socket::PMSG) -> Result<usize, SystemError> {
        // send 需要先 connect
        let peer_addr = self
            .inner
            .lock()
            .peer_endpoint()
            .ok_or(SystemError::EDESTADDRREQ)?;

        let nonblock = self.is_nonblocking() || flags.contains(PMSG::DONTWAIT);

        if nonblock {
            return Self::convert_enobufs_to_eagain(self.try_send_to(buffer, &peer_addr));
        }

        loop {
            match self.try_send_to(buffer, &peer_addr) {
                Ok(len) => return Ok(len),
                Err(SystemError::ENOBUFS) => {
                    self.wait_event_interruptible_timeout(
                        || self.send_buffer_available(buffer.len()),
                        self.send_timeout(),
                    )?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn send_msg(&self, _msg: &MsgHdr, _flags: socket::PMSG) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: socket::PMSG,
        address: Endpoint,
    ) -> Result<usize, SystemError> {
        let unix_endpoint = UnixEndpoint::try_from(address)?;
        let target_addr = unix_endpoint.connect()?;

        let nonblock = self.is_nonblocking() || flags.contains(PMSG::DONTWAIT);

        if nonblock {
            return Self::convert_enobufs_to_eagain(self.try_send_to(buffer, &target_addr));
        }

        loop {
            match self.try_send_to(buffer, &target_addr) {
                Ok(len) => return Ok(len),
                Err(SystemError::ENOBUFS) => {
                    self.wait_event_interruptible_timeout(
                        || self.send_buffer_available(buffer.len()),
                        self.send_timeout(),
                    )?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn send_buffer_size(&self) -> usize {
        self.sndbuf.load(Ordering::Relaxed)
    }

    fn recv_buffer_size(&self) -> usize {
        self.rcvbuf.load(Ordering::Relaxed)
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epitems
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        if !matches!(level, PSOL::SOCKET) {
            return Err(SystemError::ENOPROTOOPT);
        }

        let opt =
            crate::net::socket::PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
        match opt {
            crate::net::socket::PSO::SNDBUF => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.send_buffer_size() as u32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            crate::net::socket::PSO::RCVBUF => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.recv_buffer_size() as u32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            crate::net::socket::PSO::SNDTIMEO_OLD | crate::net::socket::PSO::SNDTIMEO_NEW => {
                Self::write_timeval(value, self.send_timeout_us.load(Ordering::Relaxed))
            }
            crate::net::socket::PSO::RCVTIMEO_OLD | crate::net::socket::PSO::RCVTIMEO_NEW => {
                Self::write_timeval(value, self.recv_timeout_us.load(Ordering::Relaxed))
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn do_close(&self) -> Result<(), SystemError> {
        // 先从绑定表中注销，防止新消息进入
        let inner = self.inner.lock();
        if let Some(ref addr) = inner.local_addr {
            BIND_TABLE.unregister(addr);
        }
        drop(inner);

        // 清空接收队列并释放所有发送端记账
        // 注意：需要在获取 inner 锁之后操作，但释放记账时不能持有 inner 锁
        // 因为 release_sender_accounting 可能需要查找 BIND_TABLE
        let messages_to_release: Vec<(Option<UnixEndpointBound>, usize)> = {
            let mut inner = self.inner.lock();
            inner
                .recv_queue
                .drain(..)
                .map(|msg| (msg.sender_addr, msg.sender_accounted_len))
                .collect()
        };

        // 释放所有发送端记账（不持有 inner 锁）
        for (sender_addr, accounted_len) in messages_to_release {
            Self::release_sender_accounting(&sender_addr, accounted_len);
        }

        Ok(())
    }

    fn shutdown(&self, _how: socket::common::ShutdownBit) -> Result<(), SystemError> {
        if _how.is_recv_shutdown() {
            self.is_read_shutdown.store(true, Ordering::Relaxed);
        }
        if _how.is_send_shutdown() {
            self.is_write_shutdown.store(true, Ordering::Relaxed);
        }

        self.wait_queue
            .wakeup(Some(crate::process::ProcessState::Blocked(true)));
        Ok(())
    }

    fn check_io_event(&self) -> crate::filesystem::epoll::EPollEventType {
        use crate::filesystem::epoll::EPollEventType;
        let mut events = EPollEventType::empty();

        let inner = self.inner.lock();
        if inner.has_message() {
            events |= EPollEventType::EPOLLIN;
        }
        // 数据报 socket 总是可写的（除非队列满）
        events |= EPollEventType::EPOLLOUT;

        events
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
}

impl Drop for UnixDatagramSocket {
    fn drop(&mut self) {
        // 从绑定表中注销，释放地址
        let inner = self.inner.lock();
        if let Some(ref addr) = inner.local_addr {
            BIND_TABLE.unregister(addr);
        }
        // 注意：不在这里释放接收队列中的记账
        // 因为 Drop 运行时其他 socket 可能已经被 drop，访问它们不安全
        // 正常的关闭流程应该通过 do_close 完成
    }
}
