use core::{any::Any, fmt::Debug, sync::atomic::AtomicUsize};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::{self, tcp, udp},
};
use system_error::SystemError;

use crate::{
    arch::{rand::rand, sched::sched},
    filesystem::vfs::{
        file::FileMode, syscall::ModeType, FilePrivateData, FileSystem, FileType, IndexNode,
        Metadata,
    },
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::EventWaitQueue,
    },
};

use self::sockets::{RawSocket, SeqpacketSocket, TcpSocket, UdpSocket};

use super::{
    event_poll::{EPollEventType, EPollItem, EventPoll},
    net_core::poll_ifaces,
    Endpoint, Protocol, ShutdownType,
};

pub mod sockets;

lazy_static! {
    /// 所有socket的集合
    /// TODO: 优化这里，自己实现SocketSet！！！现在这样的话，不管全局有多少个网卡，每个时间点都只会有1个进程能够访问socket
    pub static ref SOCKET_SET: SpinLock<SocketSet<'static >> = SpinLock::new(SocketSet::new(vec![]));
    /// SocketHandle表，每个SocketHandle对应一个SocketHandleItem，
    /// 注意！：在网卡中断中需要拿到这张表的🔓，在获取读锁时应该确保关中断避免死锁
    pub static ref HANDLE_MAP: RwLock<HashMap<SocketHandle, SocketHandleItem>> = RwLock::new(HashMap::new());
    /// 端口管理器
    pub static ref PORT_MANAGER: PortManager = PortManager::new();
}

/* For setsockopt(2) */
// See: linux-5.19.10/include/uapi/asm-generic/socket.h#9
pub const SOL_SOCKET: u8 = 1;

/// 根据地址族、socket类型和协议创建socket
pub(super) fn new_socket(
    address_family: AddressFamily,
    socket_type: PosixSocketType,
    protocol: Protocol,
) -> Result<Box<dyn Socket>, SystemError> {
    let socket: Box<dyn Socket> = match address_family {
        AddressFamily::Unix => match socket_type {
            PosixSocketType::Stream => Box::new(TcpSocket::new(SocketOptions::default())),
            PosixSocketType::Datagram => Box::new(UdpSocket::new(SocketOptions::default())),
            PosixSocketType::Raw => Box::new(RawSocket::new(protocol, SocketOptions::default())),
            PosixSocketType::SeqPacket => Box::new(SeqpacketSocket::new(SocketOptions::default())),
            _ => {
                return Err(SystemError::EINVAL);
            }
        },
        AddressFamily::INet => match socket_type {
            PosixSocketType::Stream => Box::new(TcpSocket::new(SocketOptions::default())),
            PosixSocketType::Datagram => Box::new(UdpSocket::new(SocketOptions::default())),
            PosixSocketType::Raw => Box::new(RawSocket::new(protocol, SocketOptions::default())),
            _ => {
                return Err(SystemError::EINVAL);
            }
        },
        _ => {
            return Err(SystemError::EAFNOSUPPORT);
        }
    };
    Ok(socket)
}

pub trait Socket: Sync + Send + Debug + Any {
    fn as_any_ref(&self) -> &dyn Any;

    fn as_any_mut(&mut self) -> &mut dyn Any;
    /// @brief 从socket中读取数据，如果socket是阻塞的，那么直到读取到数据才返回
    ///
    /// @param buf 读取到的数据存放的缓冲区
    ///
    /// @return - 成功：(返回读取的数据的长度，读取数据的端点).
    ///         - 失败：错误码
    fn read(&mut self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint);

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
    fn connect(&mut self, _endpoint: Endpoint) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

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
    fn shutdown(&mut self, _type: ShutdownType) -> Result<(), SystemError> {
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

    fn socketpair_ops(&self) -> Option<&'static dyn SocketpairOps> {
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
        return EPollEventType::empty();
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
        return Ok(());
    }

    fn socket_handle(&self) -> SocketHandle {
        todo!()
    }

    fn add_epoll(&mut self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        HANDLE_MAP
            .write_irqsave()
            .get_mut(&self.socket_handle())
            .unwrap()
            .add_epoll(epitem);
        Ok(())
    }

    fn remove_epoll(&mut self, epoll: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        HANDLE_MAP
            .write_irqsave()
            .get_mut(&self.socket_handle())
            .unwrap()
            .remove_epoll(epoll)?;

        Ok(())
    }

    fn clear_epoll(&mut self) -> Result<(), SystemError> {
        let mut handle_map_guard = HANDLE_MAP.write_irqsave();
        let handle_item = handle_map_guard.get_mut(&self.socket_handle()).unwrap();

        for epitem in handle_item.epitems.lock_irqsave().iter() {
            let epoll = epitem.epoll();
            if epoll.upgrade().is_some() {
                EventPoll::ep_remove(
                    &mut epoll.upgrade().unwrap().lock_irqsave(),
                    epitem.fd(),
                    None,
                )?;
            }
        }

        Ok(())
    }
}

impl Clone for Box<dyn Socket> {
    fn clone(&self) -> Box<dyn Socket> {
        self.box_clone()
    }
}

pub trait SocketpairOps {
    /// 执行socketpair
    fn socketpair(&self, socket0: &mut Box<dyn Socket>, socket1: &mut Box<dyn Socket>);
}

/// # Socket在文件系统中的inode封装
#[derive(Debug)]
pub struct SocketInode(SpinLock<Box<dyn Socket>>, AtomicUsize);

impl SocketInode {
    pub fn new(socket: Box<dyn Socket>) -> Arc<Self> {
        Arc::new(Self(SpinLock::new(socket), AtomicUsize::new(0)))
    }

    #[inline]
    pub fn inner(&self) -> SpinLockGuard<Box<dyn Socket>> {
        return self.0.lock();
    }

    pub unsafe fn inner_no_preempt(&self) -> SpinLockGuard<Box<dyn Socket>> {
        return self.0.lock_no_preempt();
    }
}

impl IndexNode for SocketInode {
    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        self.1.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        let prev_ref_count = self.1.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
        if prev_ref_count == 1 {
            // 最后一次关闭，需要释放
            let mut socket = self.0.lock_irqsave();

            if socket.metadata().unwrap().socket_type == SocketType::SeqpacketSocket {
                return Ok(());
            }

            if let Some(Endpoint::Ip(Some(ip))) = socket.endpoint() {
                PORT_MANAGER.unbind_port(socket.metadata().unwrap().socket_type, ip.port)?;
            }

            socket.clear_epoll()?;

            HANDLE_MAP
                .write_irqsave()
                .remove(&socket.socket_handle())
                .unwrap();
        }
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        return self.0.lock_no_preempt().read(&mut buf[0..len]).0;
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        return self.0.lock_no_preempt().write(&buf[0..len], None);
    }

    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let events = self.0.lock_irqsave().poll();
        return Ok(events.bits() as usize);
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        return Err(SystemError::ENOTDIR);
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let meta = Metadata {
            mode: ModeType::from_bits_truncate(0o755),
            file_type: FileType::Socket,
            ..Default::default()
        };

        return Ok(meta);
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        return Ok(());
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct SocketHandleItem {
    /// socket元数据
    metadata: SocketMetadata,
    /// shutdown状态
    pub shutdown_type: RwLock<ShutdownType>,
    /// socket的waitqueue
    pub wait_queue: EventWaitQueue,
    /// epitems，考虑写在这是否是最优解？
    pub epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
}

impl SocketHandleItem {
    pub fn new(socket: &Box<dyn Socket>) -> Self {
        Self {
            metadata: socket.metadata().unwrap(),
            shutdown_type: RwLock::new(ShutdownType::empty()),
            wait_queue: EventWaitQueue::new(),
            epitems: SpinLock::new(LinkedList::new()),
        }
    }

    pub fn from_socket<A: Socket>(socket: &Box<A>) -> Self {
        Self {
            metadata: socket.metadata().unwrap(),
            shutdown_type: RwLock::new(ShutdownType::empty()),
            wait_queue: EventWaitQueue::new(),
            epitems: SpinLock::new(LinkedList::new()),
        }
    }

    /// ### 在socket的等待队列上睡眠
    pub fn sleep(
        socket_handle: SocketHandle,
        events: u64,
        handle_map_guard: RwLockReadGuard<'_, HashMap<SocketHandle, SocketHandleItem>>,
    ) {
        unsafe {
            handle_map_guard
                .get(&socket_handle)
                .unwrap()
                .wait_queue
                .sleep_without_schedule(events)
        };
        drop(handle_map_guard);
        sched();
    }

    pub fn shutdown_type(&self) -> ShutdownType {
        self.shutdown_type.read().clone()
    }

    pub fn shutdown_type_writer(&mut self) -> RwLockWriteGuard<ShutdownType> {
        self.shutdown_type.write_irqsave()
    }

    pub fn add_epoll(&mut self, epitem: Arc<EPollItem>) {
        self.epitems.lock_irqsave().push_back(epitem)
    }

    pub fn remove_epoll(&mut self, epoll: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        let is_remove = !self
            .epitems
            .lock_irqsave()
            .extract_if(|x| x.epoll().ptr_eq(epoll))
            .collect::<Vec<_>>()
            .is_empty();

        if is_remove {
            return Ok(());
        }

        Err(SystemError::ENOENT)
    }
}

/// # TCP 和 UDP 的端口管理器。
/// 如果 TCP/UDP 的 socket 绑定了某个端口，它会在对应的表中记录，以检测端口冲突。
pub struct PortManager {
    // TCP 端口记录表
    tcp_port_table: SpinLock<HashMap<u16, Arc<GlobalSocketHandle>>>,
    // UDP 端口记录表
    udp_port_table: SpinLock<HashMap<u16, Arc<GlobalSocketHandle>>>,
}

impl PortManager {
    pub fn new() -> Self {
        return Self {
            tcp_port_table: SpinLock::new(HashMap::new()),
            udp_port_table: SpinLock::new(HashMap::new()),
        };
    }

    /// @brief 自动分配一个相对应协议中未被使用的PORT，如果动态端口均已被占用，返回错误码 EADDRINUSE
    pub fn get_ephemeral_port(&self, socket_type: SocketType) -> Result<u16, SystemError> {
        // TODO: selects non-conflict high port

        static mut EPHEMERAL_PORT: u16 = 0;
        unsafe {
            if EPHEMERAL_PORT == 0 {
                EPHEMERAL_PORT = (49152 + rand() % (65536 - 49152)) as u16;
            }
        }

        let mut remaining = 65536 - 49152; // 剩余尝试分配端口次数
        let mut port: u16;
        while remaining > 0 {
            unsafe {
                if EPHEMERAL_PORT == 65535 {
                    EPHEMERAL_PORT = 49152;
                } else {
                    EPHEMERAL_PORT = EPHEMERAL_PORT + 1;
                }
                port = EPHEMERAL_PORT;
            }

            // 使用 ListenTable 检查端口是否被占用
            let listen_table_guard = match socket_type {
                SocketType::UdpSocket => self.udp_port_table.lock(),
                SocketType::TcpSocket => self.tcp_port_table.lock(),
                _ => panic!("{:?} cann't get a port", socket_type),
            };
            if listen_table_guard.get(&port).is_none() {
                drop(listen_table_guard);
                return Ok(port);
            }
            remaining -= 1;
        }
        return Err(SystemError::EADDRINUSE);
    }

    /// @brief 检测给定端口是否已被占用，如果未被占用则在 TCP/UDP 对应的表中记录
    ///
    /// TODO: 增加支持端口复用的逻辑
    pub fn bind_port(
        &self,
        socket_type: SocketType,
        port: u16,
        handle: Arc<GlobalSocketHandle>,
    ) -> Result<(), SystemError> {
        if port > 0 {
            let mut listen_table_guard = match socket_type {
                SocketType::UdpSocket => self.udp_port_table.lock(),
                SocketType::TcpSocket => self.tcp_port_table.lock(),
                _ => panic!("{:?} cann't bind a port", socket_type),
            };
            match listen_table_guard.get(&port) {
                Some(_) => return Err(SystemError::EADDRINUSE),
                None => listen_table_guard.insert(port, handle),
            };
            drop(listen_table_guard);
        }
        return Ok(());
    }

    /// @brief 在对应的端口记录表中将端口和 socket 解绑
    pub fn unbind_port(&self, socket_type: SocketType, port: u16) -> Result<(), SystemError> {
        let mut listen_table_guard = match socket_type {
            SocketType::UdpSocket => self.udp_port_table.lock(),
            SocketType::TcpSocket => self.tcp_port_table.lock(),
            _ => return Ok(()),
        };
        listen_table_guard.remove(&port);
        drop(listen_table_guard);
        return Ok(());
    }
}

/// # socket的句柄管理组件
/// 它在smoltcp的SocketHandle上封装了一层，增加更多的功能。
/// 比如，在socket被关闭时，自动释放socket的资源，通知系统的其他组件。
#[derive(Debug)]
pub struct GlobalSocketHandle(SocketHandle);

impl GlobalSocketHandle {
    pub fn new(handle: SocketHandle) -> Arc<Self> {
        return Arc::new(Self(handle));
    }
}

impl Clone for GlobalSocketHandle {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for GlobalSocketHandle {
    fn drop(&mut self) {
        let mut socket_set_guard = SOCKET_SET.lock_irqsave();
        socket_set_guard.remove(self.0); // 删除的时候，会发送一条FINISH的信息？
        drop(socket_set_guard);
        poll_ifaces();
    }
}

/// @brief socket的类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SocketType {
    /// 原始的socket
    RawSocket,
    /// 用于Tcp通信的 Socket
    TcpSocket,
    /// 用于Udp通信的 Socket
    UdpSocket,
    /// 用于进程间通信的 Socket
    SeqpacketSocket,
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

#[derive(Debug, Clone)]
/// @brief 在trait Socket的metadata函数中返回该结构体供外部使用
pub struct SocketMetadata {
    /// socket的类型
    pub socket_type: SocketType,
    /// 接收缓冲区的大小
    pub rx_buf_size: usize,
    /// 发送缓冲区的大小
    pub tx_buf_size: usize,
    /// 元数据的缓冲区的大小
    pub metadata_buf_size: usize,
    /// socket的选项
    pub options: SocketOptions,
}

impl SocketMetadata {
    fn new(
        socket_type: SocketType,
        rx_buf_size: usize,
        tx_buf_size: usize,
        metadata_buf_size: usize,
        options: SocketOptions,
    ) -> Self {
        Self {
            socket_type,
            rx_buf_size,
            tx_buf_size,
            metadata_buf_size,
            options,
        }
    }
}

/// @brief 地址族的枚举
///
/// 参考：https://code.dragonos.org.cn/xref/linux-5.19.10/include/linux/socket.h#180
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum AddressFamily {
    /// AF_UNSPEC 表示地址族未指定
    Unspecified = 0,
    /// AF_UNIX 表示Unix域的socket (与AF_LOCAL相同)
    Unix = 1,
    ///  AF_INET 表示IPv4的socket
    INet = 2,
    /// AF_AX25 表示AMPR AX.25的socket
    AX25 = 3,
    /// AF_IPX 表示IPX的socket
    IPX = 4,
    /// AF_APPLETALK 表示Appletalk的socket
    Appletalk = 5,
    /// AF_NETROM 表示AMPR NET/ROM的socket
    Netrom = 6,
    /// AF_BRIDGE 表示多协议桥接的socket
    Bridge = 7,
    /// AF_ATMPVC 表示ATM PVCs的socket
    Atmpvc = 8,
    /// AF_X25 表示X.25的socket
    X25 = 9,
    /// AF_INET6 表示IPv6的socket
    INet6 = 10,
    /// AF_ROSE 表示AMPR ROSE的socket
    Rose = 11,
    /// AF_DECnet Reserved for DECnet project
    Decnet = 12,
    /// AF_NETBEUI Reserved for 802.2LLC project
    Netbeui = 13,
    /// AF_SECURITY 表示Security callback的伪AF
    Security = 14,
    /// AF_KEY 表示Key management API
    Key = 15,
    /// AF_NETLINK 表示Netlink的socket
    Netlink = 16,
    /// AF_PACKET 表示Low level packet interface
    Packet = 17,
    /// AF_ASH 表示Ash
    Ash = 18,
    /// AF_ECONET 表示Acorn Econet
    Econet = 19,
    /// AF_ATMSVC 表示ATM SVCs
    Atmsvc = 20,
    /// AF_RDS 表示Reliable Datagram Sockets
    Rds = 21,
    /// AF_SNA 表示Linux SNA Project
    Sna = 22,
    /// AF_IRDA 表示IRDA sockets
    Irda = 23,
    /// AF_PPPOX 表示PPPoX sockets
    Pppox = 24,
    /// AF_WANPIPE 表示WANPIPE API sockets
    WanPipe = 25,
    /// AF_LLC 表示Linux LLC
    Llc = 26,
    /// AF_IB 表示Native InfiniBand address
    /// 介绍：https://access.redhat.com/documentation/en-us/red_hat_enterprise_linux/9/html-single/configuring_infiniband_and_rdma_networks/index#understanding-infiniband-and-rdma_configuring-infiniband-and-rdma-networks
    Ib = 27,
    /// AF_MPLS 表示MPLS
    Mpls = 28,
    /// AF_CAN 表示Controller Area Network
    Can = 29,
    /// AF_TIPC 表示TIPC sockets
    Tipc = 30,
    /// AF_BLUETOOTH 表示Bluetooth sockets
    Bluetooth = 31,
    /// AF_IUCV 表示IUCV sockets
    Iucv = 32,
    /// AF_RXRPC 表示RxRPC sockets
    Rxrpc = 33,
    /// AF_ISDN 表示mISDN sockets
    Isdn = 34,
    /// AF_PHONET 表示Phonet sockets
    Phonet = 35,
    /// AF_IEEE802154 表示IEEE 802.15.4 sockets
    Ieee802154 = 36,
    /// AF_CAIF 表示CAIF sockets
    Caif = 37,
    /// AF_ALG 表示Algorithm sockets
    Alg = 38,
    /// AF_NFC 表示NFC sockets
    Nfc = 39,
    /// AF_VSOCK 表示vSockets
    Vsock = 40,
    /// AF_KCM 表示Kernel Connection Multiplexor
    Kcm = 41,
    /// AF_QIPCRTR 表示Qualcomm IPC Router
    Qipcrtr = 42,
    /// AF_SMC 表示SMC-R sockets.
    /// reserve number for PF_SMC protocol family that reuses AF_INET address family
    Smc = 43,
    /// AF_XDP 表示XDP sockets
    Xdp = 44,
    /// AF_MCTP 表示Management Component Transport Protocol
    Mctp = 45,
    /// AF_MAX 表示最大的地址族
    Max = 46,
}

impl TryFrom<u16> for AddressFamily {
    type Error = SystemError;
    fn try_from(x: u16) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u16(x).ok_or_else(|| SystemError::EINVAL);
    }
}

/// @brief posix套接字类型的枚举(这些值与linux内核中的值一致)
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum PosixSocketType {
    Stream = 1,
    Datagram = 2,
    Raw = 3,
    Rdm = 4,
    SeqPacket = 5,
    Dccp = 6,
    Packet = 10,
}

impl TryFrom<u8> for PosixSocketType {
    type Error = SystemError;
    fn try_from(x: u8) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u8(x).ok_or_else(|| SystemError::EINVAL);
    }
}

/// ### 为socket提供无锁的poll方法
///
/// 因为在网卡中断中，需要轮询socket的状态，如果使用socket文件或者其inode来poll
/// 在当前的设计，会必然死锁，所以引用这一个设计来解决，提供无🔓的poll
pub struct SocketPollMethod;

impl SocketPollMethod {
    pub fn poll(socket: &socket::Socket, shutdown: ShutdownType) -> EPollEventType {
        match socket {
            socket::Socket::Udp(udp) => Self::udp_poll(udp, shutdown),
            socket::Socket::Tcp(tcp) => Self::tcp_poll(tcp, shutdown),
            _ => todo!(),
        }
    }

    pub fn tcp_poll(socket: &tcp::Socket, shutdown: ShutdownType) -> EPollEventType {
        let mut events = EPollEventType::empty();
        if socket.is_listening() && socket.is_active() {
            events.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
            return events;
        }

        // socket已经关闭
        if !socket.is_open() {
            events.insert(EPollEventType::EPOLLHUP)
        }
        if shutdown.contains(ShutdownType::RCV_SHUTDOWN) {
            events.insert(
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM | EPollEventType::EPOLLRDHUP,
            );
        }

        let state = socket.state();
        if state != tcp::State::SynSent && state != tcp::State::SynReceived {
            // socket有可读数据
            if socket.can_recv() {
                events.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
            }

            if !(shutdown.contains(ShutdownType::SEND_SHUTDOWN)) {
                // 缓冲区可写
                if socket.send_queue() < socket.send_capacity() {
                    events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
                } else {
                    // TODO：触发缓冲区已满的信号
                    todo!("A signal that the buffer is full needs to be sent");
                }
            } else {
                // 如果我们的socket关闭了SEND_SHUTDOWN，epoll事件就是EPOLLOUT
                events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
            }
        } else if state == tcp::State::SynSent {
            events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
        }

        // socket发生错误
        if !socket.is_active() {
            events.insert(EPollEventType::EPOLLERR);
        }

        events
    }

    pub fn udp_poll(socket: &udp::Socket, shutdown: ShutdownType) -> EPollEventType {
        let mut event = EPollEventType::empty();

        if shutdown.contains(ShutdownType::RCV_SHUTDOWN) {
            event.insert(
                EPollEventType::EPOLLRDHUP | EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        }
        if shutdown.contains(ShutdownType::SHUTDOWN_MASK) {
            event.insert(EPollEventType::EPOLLHUP);
        }

        if socket.can_recv() {
            event.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
        }

        if socket.can_send() {
            event.insert(
                EPollEventType::EPOLLOUT
                    | EPollEventType::EPOLLWRNORM
                    | EPollEventType::EPOLLWRBAND,
            );
        } else {
            // TODO: 缓冲区空间不够，需要使用信号处理
            todo!()
        }

        return event;
    }
}
