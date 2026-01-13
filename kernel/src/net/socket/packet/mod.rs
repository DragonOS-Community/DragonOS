//! AF_PACKET Socket 实现
//!
//! 提供 L2 层数据包访问，用于 tcpdump、wireshark 等抓包工具

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::mutex::Mutex;
use crate::libs::rwsem::RwSem;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::EPollItems;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::{Socket, PMSG, PSOCK, PSOL};
use crate::process::cred::CAPFlags;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;

type EP = crate::filesystem::epoll::EPollEventType;

/// 以太网协议类型常量
#[allow(dead_code)]
pub mod eth_protocol {
    /// 接收所有协议的数据包
    pub const ETH_P_ALL: u16 = 0x0003;
    /// IP 协议
    pub const ETH_P_IP: u16 = 0x0800;
    /// ARP 协议
    pub const ETH_P_ARP: u16 = 0x0806;
    /// IPv6 协议
    pub const ETH_P_IPV6: u16 = 0x86DD;
}

/// 数据包类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    /// 发往本机的包
    Host = 0,
    /// 广播包
    Broadcast = 1,
    /// 多播包
    Multicast = 2,
    /// 发往其他主机的包（混杂模式下捕获）
    OtherHost = 3,
    /// 本机发出的包
    Outgoing = 4,
    /// 环回包
    Loopback = 5,
}

impl Default for PacketType {
    fn default() -> Self {
        Self::Host
    }
}

/// AF_PACKET socket 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketSocketType {
    /// SOCK_RAW: 包含链路层头
    Raw,
    /// SOCK_DGRAM: 不包含链路层头
    Dgram,
}

/// 接收到的数据包元数据
#[derive(Debug, Clone, Default)]
pub struct PacketMetadata {
    /// 源 MAC 地址
    pub src_mac: [u8; 6],
    /// 目标 MAC 地址 (用于调试和诊断)
    #[allow(dead_code)]
    pub dst_mac: [u8; 6],
    /// 协议号 (网络字节序)
    pub protocol: u16,
    /// 接口索引
    pub ifindex: u32,
    /// 包类型
    pub pkt_type: PacketType,
}

/// 接收缓冲区中的数据包
#[derive(Debug)]
pub struct ReceivedPacket {
    pub data: Vec<u8>,
    pub metadata: PacketMetadata,
}

/// sockaddr_ll 结构 (用于 AF_PACKET 地址)
#[derive(Debug, Clone, Default)]
#[repr(C)]
pub struct SockAddrLl {
    /// 地址族 (AF_PACKET = 17)
    pub sll_family: u16,
    /// 协议号 (网络字节序)
    pub sll_protocol: u16,
    /// 接口索引
    pub sll_ifindex: i32,
    /// 硬件类型
    pub sll_hatype: u16,
    /// 包类型
    pub sll_pkttype: u8,
    /// 地址长度
    pub sll_halen: u8,
    /// MAC 地址 (最多 8 字节)
    pub sll_addr: [u8; 8],
}

/// Packet socket 选项 (为将来的功能预留)
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct PacketSocketOptions {
    /// 是否启用辅助数据
    pub auxdata: bool,
    /// 接收时间戳
    pub timestamp: bool,
}

const DEFAULT_RX_BUFFER_PACKETS: usize = 256;
const DEFAULT_RX_BUFFER_SIZE: usize = 256 * 1024; // 256KB

/// PacketSocket - AF_PACKET 实现
///
/// 提供 L2 层数据包访问，支持：
/// - SOCK_RAW: 包含完整以太网帧
/// - SOCK_DGRAM: 不包含以太网头
#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct PacketSocket {
    /// socket 类型 (SOCK_RAW 或 SOCK_DGRAM)
    sock_type: PacketSocketType,
    /// 绑定的协议号 (ETH_P_ALL = 0x0003 表示接收所有协议)
    protocol: u16,
    /// 绑定的网络接口
    bound_iface: RwSem<Option<Arc<dyn Iface>>>,
    /// 接收缓冲区
    rx_buffer: Mutex<VecDeque<ReceivedPacket>>,
    /// 接收缓冲区最大包数
    rx_buffer_max_packets: AtomicUsize,
    /// 选项 (为将来的功能预留)
    #[allow(dead_code)]
    options: RwSem<PacketSocketOptions>,
    /// 非阻塞标志
    nonblock: AtomicBool,
    /// 等待队列
    wait_queue: WaitQueue,
    /// inode id
    inode_id: InodeId,
    /// 打开文件计数
    open_files: AtomicUsize,
    /// 自引用
    self_ref: Weak<Self>,
    /// 网络命名空间
    netns: Arc<NetNamespace>,
    /// epoll 项
    epoll_items: EPollItems,
    /// fasync 项
    fasync_items: FAsyncItems,
}

impl PacketSocket {
    /// 创建新的 packet socket
    ///
    /// # 权限检查
    /// 需要 CAP_NET_RAW 权限
    pub fn new(sock_type: PSOCK, protocol: u16, nonblock: bool) -> Result<Arc<Self>, SystemError> {
        // CAP_NET_RAW 权限检查
        let cred = ProcessManager::current_pcb().cred();
        if !cred.has_capability(CAPFlags::CAP_NET_RAW) {
            log::warn!("PacketSocket::new: CAP_NET_RAW check failed");
            return Err(SystemError::EPERM);
        }

        let socket_type = match sock_type {
            PSOCK::Raw => PacketSocketType::Raw,
            PSOCK::Datagram => PacketSocketType::Dgram,
            _ => return Err(SystemError::ESOCKTNOSUPPORT),
        };

        let netns = ProcessManager::current_netns();

        Ok(Arc::new_cyclic(|me| Self {
            sock_type: socket_type,
            protocol,
            bound_iface: RwSem::new(None),
            rx_buffer: Mutex::new(VecDeque::with_capacity(DEFAULT_RX_BUFFER_PACKETS)),
            rx_buffer_max_packets: AtomicUsize::new(DEFAULT_RX_BUFFER_PACKETS),
            options: RwSem::new(PacketSocketOptions::default()),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
        }))
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    /// 绑定到网络接口
    pub fn bind_to_interface(&self, ifindex: i32) -> Result<(), SystemError> {
        if ifindex <= 0 {
            // 解绑
            *self.bound_iface.write() = None;
            return Ok(());
        }

        let iface = self
            .netns
            .device_list()
            .values()
            .find(|iface| iface.nic_id() == ifindex as usize)
            .cloned()
            .ok_or(SystemError::ENODEV)?;

        // 注册到网络接口以接收数据包
        iface.common().register_packet_socket(self.self_ref.clone());

        *self.bound_iface.write() = Some(iface);
        Ok(())
    }

    /// 接收数据包（由网络设备层调用）
    pub fn deliver_packet(&self, frame: &[u8], pkt_type: PacketType) {
        if frame.len() < 14 {
            return; // 以太网帧至少 14 字节
        }

        // 解析以太网头
        let dst_mac: [u8; 6] = frame[0..6].try_into().unwrap_or([0; 6]);
        let src_mac: [u8; 6] = frame[6..12].try_into().unwrap_or([0; 6]);
        let eth_type = u16::from_be_bytes([frame[12], frame[13]]);

        // 协议过滤
        // ETH_P_ALL (0x0003) 接收所有协议
        if self.protocol != eth_protocol::ETH_P_ALL && self.protocol != eth_type {
            return;
        }

        let ifindex = self
            .bound_iface
            .read()
            .as_ref()
            .map(|iface| iface.nic_id() as u32)
            .unwrap_or(0);

        let metadata = PacketMetadata {
            src_mac,
            dst_mac,
            protocol: eth_type,
            ifindex,
            pkt_type,
        };

        // 根据 socket 类型决定返回的数据
        let data = match self.sock_type {
            PacketSocketType::Raw => frame.to_vec(), // 包含以太网头
            PacketSocketType::Dgram => {
                if frame.len() > 14 {
                    frame[14..].to_vec() // 不包含以太网头
                } else {
                    Vec::new()
                }
            }
        };

        let packet = ReceivedPacket { data, metadata };

        let mut rx_buf = self.rx_buffer.lock();
        let max_packets = self.rx_buffer_max_packets.load(Ordering::Relaxed);
        if rx_buf.len() < max_packets {
            rx_buf.push_back(packet);
            drop(rx_buf);
            // 唤醒等待的进程
            self.wait_queue.wakeup(None);
        }
        // 否则丢弃（缓冲区满）
    }

    /// 尝试接收
    fn try_recv(&self, buf: &mut [u8]) -> Result<(usize, PacketMetadata), SystemError> {
        let mut rx_buf = self.rx_buffer.lock();
        if let Some(packet) = rx_buf.pop_front() {
            let copy_len = core::cmp::min(buf.len(), packet.data.len());
            buf[..copy_len].copy_from_slice(&packet.data[..copy_len]);
            Ok((copy_len, packet.metadata))
        } else {
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        }
    }

    /// 尝试发送数据包
    fn try_send(&self, buf: &[u8], dest: Option<SockAddrLl>) -> Result<usize, SystemError> {
        // 获取目标接口
        let iface = if let Some(addr) = &dest {
            if addr.sll_ifindex > 0 {
                self.netns
                    .device_list()
                    .values()
                    .find(|iface| iface.nic_id() == addr.sll_ifindex as usize)
                    .cloned()
            } else {
                self.bound_iface.read().clone()
            }
        } else {
            self.bound_iface.read().clone()
        };

        let iface = iface.ok_or(SystemError::EDESTADDRREQ)?;

        match self.sock_type {
            PacketSocketType::Raw => {
                // 用户提供完整以太网帧
                if buf.len() < 14 {
                    return Err(SystemError::EINVAL);
                }
                // 发送原始帧
                self.send_raw_frame(&iface, buf)
            }
            PacketSocketType::Dgram => {
                // 需要构造以太网头
                let dest_addr = dest.ok_or(SystemError::EDESTADDRREQ)?;
                let dest_mac: [u8; 6] = dest_addr.sll_addr[..6]
                    .try_into()
                    .map_err(|_| SystemError::EINVAL)?;

                let mut frame = Vec::with_capacity(14 + buf.len());
                // 目标 MAC
                frame.extend_from_slice(&dest_mac);
                // 源 MAC
                frame.extend_from_slice(iface.mac().as_bytes());
                // 协议类型
                let protocol = if dest_addr.sll_protocol != 0 {
                    dest_addr.sll_protocol.to_be()
                } else {
                    self.protocol.to_be()
                };
                frame.extend_from_slice(&protocol.to_be_bytes());
                // 载荷
                frame.extend_from_slice(buf);

                self.send_raw_frame(&iface, &frame)
            }
        }
    }

    /// 发送原始帧到网卡
    fn send_raw_frame(&self, iface: &Arc<dyn Iface>, frame: &[u8]) -> Result<usize, SystemError> {
        // 通过网卡接口发送原始帧
        iface.common().send_raw_packet(frame)?;
        Ok(frame.len())
    }

    #[inline]
    pub fn can_recv(&self) -> bool {
        !self.rx_buffer.lock().is_empty()
    }

    #[inline]
    #[allow(dead_code)]
    pub fn can_send(&self) -> bool {
        // 总是可以发送（除非设备不可用）
        self.bound_iface.read().is_some()
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    /// 获取自引用
    pub fn self_ref(&self) -> Weak<Self> {
        self.self_ref.clone()
    }
}

impl Socket for PacketSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::LinkLayer(ll) = endpoint {
            return self.bind_to_interface(ll.interface as i32);
        }
        Err(SystemError::EAFNOSUPPORT)
    }

    fn send_buffer_size(&self) -> usize {
        DEFAULT_RX_BUFFER_SIZE
    }

    fn recv_buffer_size(&self) -> usize {
        DEFAULT_RX_BUFFER_SIZE
    }

    fn connect(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        // AF_PACKET 不支持 connect
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
            return self.try_send(buffer, None);
        }

        // 阻塞发送
        self.try_send(buffer, None)
    }

    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError> {
        let dest = if let Endpoint::LinkLayer(ll) = &address {
            Some(SockAddrLl {
                sll_family: 17,
                sll_protocol: ll.protocol.to_be(),
                sll_ifindex: ll.interface as i32,
                sll_hatype: ll.hatype,
                sll_pkttype: ll.pkttype,
                sll_halen: ll.halen,
                sll_addr: ll.addr,
            })
        } else {
            None
        };

        if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
            return self.try_send(buffer, dest);
        }

        self.try_send(buffer, dest)
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv(buffer).map(|(len, _)| len)
        } else {
            loop {
                match self.try_recv(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    result => return result.map(|(len, _)| len),
                }
            }
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        let mut recv_fn = || {
            self.try_recv(buffer).map(|(len, metadata)| {
                let ll = crate::net::socket::endpoint::LinkLayerEndpoint {
                    interface: metadata.ifindex as usize,
                    addr: {
                        let mut addr = [0u8; 8];
                        addr[..6].copy_from_slice(&metadata.src_mac);
                        addr
                    },
                    protocol: metadata.protocol,
                    hatype: 1, // ARPHRD_ETHER
                    pkttype: metadata.pkt_type as u8,
                    halen: 6,
                };
                (len, Endpoint::LinkLayer(ll))
            })
        };

        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            recv_fn()
        } else {
            loop {
                match recv_fn() {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    result => return result,
                }
            }
        }
    }

    fn do_close(&self) -> Result<(), SystemError> {
        // 从网络接口取消注册
        if let Some(iface) = self.bound_iface.read().as_ref() {
            iface.common().unregister_packet_socket(&self.self_ref);
        }
        Ok(())
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        Err(SystemError::ENOTCONN)
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let iface = self.bound_iface.read();
        if let Some(iface) = iface.as_ref() {
            Ok(Endpoint::LinkLayer(
                crate::net::socket::endpoint::LinkLayerEndpoint {
                    interface: iface.nic_id(),
                    addr: {
                        let mut addr = [0u8; 8];
                        addr[..6].copy_from_slice(iface.mac().as_bytes());
                        addr
                    },
                    protocol: self.protocol,
                    hatype: 1, // ARPHRD_ETHER
                    pkttype: 0,
                    halen: 6,
                },
            ))
        } else {
            Ok(Endpoint::LinkLayer(
                crate::net::socket::endpoint::LinkLayerEndpoint::new(0),
            ))
        }
    }

    fn recv_msg(
        &self,
        _msg: &mut crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send_msg(
        &self,
        _msg: &crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn check_io_event(&self) -> EPollEventType {
        let mut event = EPollEventType::empty();

        if self.can_recv() {
            event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
        }

        // 总是可写（除非设备不可用）
        if self.bound_iface.read().is_some() {
            event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
        }

        event
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        if level != PSOL::PACKET {
            return Err(SystemError::ENOPROTOOPT);
        }

        match name {
            // PACKET_STATISTICS = 6
            6 => {
                // 返回统计信息（简化实现）
                if value.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                // tp_packets, tp_drops
                value[..8].fill(0);
                Ok(8)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn set_option(&self, level: PSOL, name: usize, _val: &[u8]) -> Result<(), SystemError> {
        if level != PSOL::PACKET {
            return Ok(()); // 忽略其他级别的选项
        }

        match name {
            // PACKET_ADD_MEMBERSHIP = 1
            // PACKET_DROP_MEMBERSHIP = 2
            // PACKET_AUXDATA = 8
            1 | 2 | 8 => {
                // TODO: 实现多播成员和辅助数据
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
