//! AF_PACKET Socket 实现
//!
//! 提供 L2 层数据包访问，用于 tcpdump、wireshark 等抓包工具

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::driver::net::types::InterfaceFlags;
use crate::driver::net::Iface;
use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::iov::IoVecs;
use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::mutex::Mutex;
use crate::libs::rwsem::RwSem;
use crate::libs::wait_queue::WaitQueue;
use crate::net::posix::SockAddr;
use crate::net::socket::common::{write_i32_getsockopt, write_u32_getsockopt, EPollItems};
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::unix::utils::CmsgBuffer;
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

/// SOL_PACKET 级别 socket 选项常量 (对应 Linux `include/uapi/linux/if_packet.h`)
pub mod packet_option {
    /// 加入多播组 (独立 issue)
    pub const PACKET_ADD_MEMBERSHIP: usize = 1;
    /// 离开多播组 (独立 issue)
    pub const PACKET_DROP_MEMBERSHIP: usize = 2;
    /// 获取统计信息 (只读 getsockopt)
    pub const PACKET_STATISTICS: usize = 6;
    /// 复制阈值字节数
    pub const PACKET_COPY_THRESH: usize = 7;
    /// 辅助数据开关
    pub const PACKET_AUXDATA: usize = 8;
    /// 返回原始接收接口索引
    pub const PACKET_ORIGDEV: usize = 9;
    /// TPACKET 版本 (TPACKET_V1/V2/V3)
    pub const PACKET_VERSION: usize = 10;
    /// 预留字节数
    pub const PACKET_RESERVE: usize = 12;
    /// 虚拟网络头开关
    pub const PACKET_VNET_HDR: usize = 15;
    /// 发送时间戳 fd
    pub const PACKET_TX_TIMESTAMP: usize = 16;
    /// 接收时间戳类型
    pub const PACKET_TIMESTAMP: usize = 17;
    /// QDisc 绕过开关
    pub const PACKET_QDISC_BYPASS: usize = 20;
}

/// TPACKET 版本常量 (用于 PACKET_VERSION 校验)
const TPACKET_V1: i32 = 0;
const TPACKET_V2: i32 = 1;
const TPACKET_V3: i32 = 2;

/// packet_mreq 的 mr_type 常量 (对应 Linux `include/uapi/linux/if_packet.h`)
pub mod packet_mreq_type {
    /// PACKET_MR_PROMISC: 混杂模式 (接收所有单播包)
    pub const PACKET_MR_PROMISC: u32 = 0;
    /// PACKET_MR_MULTICAST: 特定多播组
    pub const PACKET_MR_MULTICAST: u32 = 1;
    /// PACKET_MR_ALLMULTI: 所有多播
    pub const PACKET_MR_ALLMULTI: u32 = 2;
    /// PACKET_MR_UNICAST: 特定单播
    pub const PACKET_MR_UNICAST: u32 = 3;
}

/// struct packet_mreq (对应 Linux `struct packet_mreq`)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PacketMreq {
    /// 接口索引
    pub mr_ifindex: u32,
    /// 操作类型 (PACKET_MR_*)
    pub mr_type: u32,
    /// 地址长度
    pub mr_alen: u16,
    /// 硬件地址 (最多 8 字节)
    pub mr_address: [u8; 8],
}

/// SOL_PACKET 常量 (用于 cmsg level)
const SOL_PACKET: i32 = 263;

/// 数据包类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PacketType {
    /// 发往本机的包
    #[default]
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

/// struct tpacket_auxdata (对应 Linux `include/uapi/linux/if_packet.h`)
/// 用于 PACKET_AUXDATA 辅助数据
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TpacketAuxdata {
    pub tp_status: u32,
    pub tp_len: u32,
    pub tp_snaplen: u32,
    pub tp_mac: u16,
    pub tp_net: u16,
    pub tp_vlan_tci: u16,
    pub tp_vlan_tpid: u16,
}

/// Packet socket 选项存储 (对应 SOL_PACKET 级别 setsockopt/getsockopt)
#[derive(Debug, Clone, Default)]
pub struct PacketSocketOptions {
    /// PACKET_COPY_THRESH: 复制阈值字节数
    pub copy_thresh: u32,
    /// PACKET_AUXDATA: 是否启用辅助数据
    pub auxdata: bool,
    /// PACKET_ORIGDEV: 是否返回原始接收接口
    pub origdev: bool,
    /// PACKET_VERSION: TPACKET 版本 (TPACKET_V1/V2/V3)
    pub version: i32,
    /// PACKET_RESERVE: 预留字节数
    pub reserve: u32,
    /// PACKET_VNET_HDR: 是否启用虚拟网络头
    pub vnet_hdr: bool,
    /// PACKET_TX_TIMESTAMP: 发送时间戳 fd
    pub tx_timestamp: i32,
    /// PACKET_TIMESTAMP: 接收时间戳类型 (SOF_TIMESTAMPING 标志位)
    pub timestamp: i32,
    /// PACKET_QDISC_BYPASS: 是否绕过 qdisc
    pub qdisc_bypass: bool,
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
    /// 选项存储
    options: RwSem<PacketSocketOptions>,
    /// 统计: 自上次 getsockopt(PACKET_STATISTICS) 以来接收的包数
    stats_packets: AtomicU32,
    /// 统计: 自上次 getsockopt(PACKET_STATISTICS) 以来丢弃的包数
    stats_drops: AtomicU32,
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
    /// ADD_MEMBERSHIP 设置 PROMISC 的接口索引列表 (DROP 时清除)
    promisc_ifindices: Mutex<Vec<u32>>,
    /// ADD_MEMBERSHIP 设置 ALLMULTI 的接口索引列表 (DROP 时清除)
    allmulti_ifindices: Mutex<Vec<u32>>,
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
            stats_packets: AtomicU32::new(0),
            stats_drops: AtomicU32::new(0),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            promisc_ifindices: Mutex::new(Vec::new()),
            allmulti_ifindices: Mutex::new(Vec::new()),
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
            self.stats_packets.fetch_add(1, Ordering::Relaxed);
            // 唤醒等待的进程
            self.wait_queue.wakeup(None);
        } else {
            // 缓冲区满，丢弃并计数
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
        }
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

                self.send_raw_frame(&iface, &frame)?;
                // SOCK_DGRAM: 返回用户 payload 长度（非完整帧长度），符合 Linux packet_sendmsg 语义
                Ok(buf.len())
            }
        }
    }

    /// 发送原始帧到网卡
    fn send_raw_frame(&self, iface: &Arc<dyn Iface>, frame: &[u8]) -> Result<usize, SystemError> {
        // 通过网卡接口直接发送原始帧（绕过 smoltcp 协议栈）
        iface.raw_transmit(frame)?;
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

    /// 从 optval 解析 i32 (至少 4 字节)
    fn parse_i32_opt(optval: &[u8]) -> Result<i32, SystemError> {
        if optval.len() < core::mem::size_of::<i32>() {
            return Err(SystemError::EINVAL);
        }
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&optval[..4]);
        Ok(i32::from_ne_bytes(raw))
    }

    /// 从 optval 解析 u32 (至少 4 字节)
    fn parse_u32_opt(optval: &[u8]) -> Result<u32, SystemError> {
        if optval.len() < core::mem::size_of::<u32>() {
            return Err(SystemError::EINVAL);
        }
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&optval[..4]);
        Ok(u32::from_ne_bytes(raw))
    }

    /// 从 optval 解析 PacketMreq
    fn parse_mreq(optval: &[u8]) -> Result<PacketMreq, SystemError> {
        if optval.len() < core::mem::size_of::<PacketMreq>() {
            return Err(SystemError::EINVAL);
        }
        let mut raw = [0u8; core::mem::size_of::<PacketMreq>()];
        raw.copy_from_slice(&optval[..core::mem::size_of::<PacketMreq>()]);
        // SAFETY: raw is properly aligned and sized for PacketMreq (#[repr(C)])
        Ok(unsafe { core::ptr::read_unaligned(raw.as_ptr() as *const PacketMreq) })
    }

    /// 按接口索引查找网卡
    fn find_iface(&self, ifindex: u32) -> Result<Arc<dyn Iface>, SystemError> {
        self.netns
            .device_list()
            .values()
            .find(|iface| iface.nic_id() == ifindex as usize)
            .cloned()
            .ok_or(SystemError::ENODEV)
    }

    /// 处理 ADD_MEMBERSHIP / DROP_MEMBERSHIP
    fn set_membership(&self, mreq: &PacketMreq, is_add: bool) -> Result<(), SystemError> {
        match mreq.mr_type {
            packet_mreq_type::PACKET_MR_PROMISC => {
                let iface = self.find_iface(mreq.mr_ifindex)?;
                let mut flags = iface.flags();
                if is_add {
                    flags |= InterfaceFlags::PROMISC;
                    self.promisc_ifindices.lock().push(mreq.mr_ifindex);
                } else {
                    flags &= !InterfaceFlags::PROMISC;
                    self.promisc_ifindices
                        .lock()
                        .retain(|&i| i != mreq.mr_ifindex);
                }
                iface.common().set_flags(flags);
                Ok(())
            }
            packet_mreq_type::PACKET_MR_ALLMULTI => {
                let iface = self.find_iface(mreq.mr_ifindex)?;
                let mut flags = iface.flags();
                if is_add {
                    flags |= InterfaceFlags::ALLMULTI;
                    self.allmulti_ifindices.lock().push(mreq.mr_ifindex);
                } else {
                    flags &= !InterfaceFlags::ALLMULTI;
                    self.allmulti_ifindices
                        .lock()
                        .retain(|&i| i != mreq.mr_ifindex);
                }
                iface.common().set_flags(flags);
                Ok(())
            }
            packet_mreq_type::PACKET_MR_MULTICAST | packet_mreq_type::PACKET_MR_UNICAST => {
                // TODO: 多播地址过滤 — 当前接受但不处理硬件过滤
                Ok(())
            }
            _ => Err(SystemError::EINVAL),
        }
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

    fn validate_send_buffer_len(
        &self,
        len: usize,
        _address: Option<&Endpoint>,
    ) -> Result<(), SystemError> {
        if len > u16::MAX as usize {
            return Err(SystemError::EMSGSIZE);
        }
        Ok(())
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

    fn read_to_user_buffer(
        &self,
        user_buffer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        crate::net::socket::base::read_to_user_buffer_via_kernel_buf(
            self,
            user_buffer,
            self.recv_buffer_size(),
        )
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

        // 清除通过 ADD_MEMBERSHIP 设置的 PROMISC/ALLMULTI flags
        for &ifindex in self.promisc_ifindices.lock().iter() {
            if let Ok(iface) = self.find_iface(ifindex) {
                let mut flags = iface.flags();
                flags &= !InterfaceFlags::PROMISC;
                iface.common().set_flags(flags);
            }
        }
        for &ifindex in self.allmulti_ifindices.lock().iter() {
            if let Ok(iface) = self.find_iface(ifindex) {
                let mut flags = iface.flags();
                flags &= !InterfaceFlags::ALLMULTI;
                iface.common().set_flags(flags);
            }
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
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let mut buf = iovs.new_buf(true)?;
        let buf_cap = buf.len();

        let (copy_len, metadata) = if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            self.try_recv(&mut buf)?
        } else {
            loop {
                match self.try_recv(&mut buf) {
                    Ok(r) => break r,
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        // Scatter 到用户 iovec
        iovs.scatter(&buf[..copy_len])?;

        // 写 sockaddr_ll (msg_name)
        if !msg.msg_name.is_null() {
            let mut sll_addr = [0u8; 8];
            sll_addr[..6].copy_from_slice(&metadata.src_mac);
            let sll = SockAddrLl {
                sll_family: 17, // AF_PACKET
                sll_protocol: metadata.protocol.to_be(),
                sll_ifindex: metadata.ifindex as i32,
                sll_hatype: 1, // ARPHRD_ETHER
                sll_pkttype: metadata.pkt_type as u8,
                sll_halen: 6,
                sll_addr,
            };
            let sll_bytes = unsafe {
                core::slice::from_raw_parts(
                    &sll as *const SockAddrLl as *const u8,
                    core::mem::size_of::<SockAddrLl>(),
                )
            };
            let mut writer = crate::syscall::user_access::UserBufferWriter::new(
                msg.msg_name as *mut u8,
                core::mem::size_of::<SockAddrLl>(),
                true,
            )?;
            writer.buffer_protected(0)?.write_to_user(0, sll_bytes)?;
            msg.msg_namelen = core::mem::size_of::<SockAddrLl>() as u32;
        } else {
            msg.msg_namelen = 0;
        }

        // 设置 msg_flags (截断时 MSG_TRUNC)
        let cmsg_len = msg.msg_controllen;
        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        let orig_len = copy_len; // AF_PACKET 不跟踪原始长度，用 copy_len
        if orig_len > buf_cap {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }

        // 写 PACKET_AUXDATA cmsg (当启用且 msg_control 不为 null)
        let auxdata_enabled = self.options.read().auxdata;
        if auxdata_enabled && cmsg_len > 0 {
            let aux = TpacketAuxdata {
                tp_status: 0,
                tp_len: copy_len as u32,
                tp_snaplen: copy_len as u32,
                tp_mac: 0,
                tp_net: 0,
                tp_vlan_tci: 0,
                tp_vlan_tpid: 0,
            };
            let aux_bytes = unsafe {
                core::slice::from_raw_parts(
                    &aux as *const TpacketAuxdata as *const u8,
                    core::mem::size_of::<TpacketAuxdata>(),
                )
            };
            let mut write_off = 0usize;
            let mut cmsg_buf = CmsgBuffer {
                ptr: msg.msg_control,
                len: cmsg_len,
                write_off: &mut write_off,
            };
            cmsg_buf.put(
                &mut msg.msg_flags,
                SOL_PACKET,
                packet_option::PACKET_AUXDATA as i32,
                core::mem::size_of::<TpacketAuxdata>(),
                aux_bytes,
            )?;
            msg.msg_controllen = write_off;
        }

        let ret_len = if flags.contains(PMSG::TRUNC) {
            orig_len
        } else {
            copy_len
        };
        Ok(ret_len)
    }

    fn send_msg(&self, msg: &crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let data = iovs.gather()?;

        // 解析目标地址
        let dest = if !msg.msg_name.is_null() && msg.msg_namelen > 0 {
            let endpoint = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            if let Endpoint::LinkLayer(ll) = endpoint {
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
                return Err(SystemError::EINVAL);
            }
        } else {
            None
        };

        self.try_send(&data, dest)
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

        let opts = self.options.read();
        match name {
            packet_option::PACKET_STATISTICS => {
                // struct tpacket_stats { tp_packets: u32, tp_drops: u32 }
                // Linux 语义: 返回自上次 getsockopt 以来的累计统计并清零计数器
                if value.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                let packets = self.stats_packets.swap(0, Ordering::Relaxed);
                let drops = self.stats_drops.swap(0, Ordering::Relaxed);
                value[..4].copy_from_slice(&packets.to_ne_bytes());
                value[4..8].copy_from_slice(&drops.to_ne_bytes());
                Ok(8)
            }
            packet_option::PACKET_COPY_THRESH => Ok(write_u32_getsockopt(value, opts.copy_thresh)),
            packet_option::PACKET_AUXDATA => Ok(write_i32_getsockopt(value, opts.auxdata as i32)),
            packet_option::PACKET_ORIGDEV => Ok(write_i32_getsockopt(value, opts.origdev as i32)),
            packet_option::PACKET_VERSION => Ok(write_i32_getsockopt(value, opts.version)),
            packet_option::PACKET_RESERVE => Ok(write_u32_getsockopt(value, opts.reserve)),
            packet_option::PACKET_VNET_HDR => Ok(write_i32_getsockopt(value, opts.vnet_hdr as i32)),
            packet_option::PACKET_TX_TIMESTAMP => {
                Ok(write_i32_getsockopt(value, opts.tx_timestamp))
            }
            packet_option::PACKET_TIMESTAMP => Ok(write_i32_getsockopt(value, opts.timestamp)),
            packet_option::PACKET_QDISC_BYPASS => {
                Ok(write_i32_getsockopt(value, opts.qdisc_bypass as i32))
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        if level != PSOL::PACKET {
            return Err(SystemError::ENOPROTOOPT);
        }

        // ADD/DROP_MEMBERSHIP 不需要 options 写锁
        match name {
            packet_option::PACKET_ADD_MEMBERSHIP | packet_option::PACKET_DROP_MEMBERSHIP => {
                let mreq = Self::parse_mreq(val)?;
                return self.set_membership(&mreq, name == packet_option::PACKET_ADD_MEMBERSHIP);
            }
            _ => {}
        }

        let mut opts = self.options.write();
        match name {
            packet_option::PACKET_COPY_THRESH => {
                opts.copy_thresh = Self::parse_u32_opt(val)?;
                Ok(())
            }
            packet_option::PACKET_AUXDATA => {
                opts.auxdata = Self::parse_i32_opt(val)? != 0;
                Ok(())
            }
            packet_option::PACKET_ORIGDEV => {
                opts.origdev = Self::parse_i32_opt(val)? != 0;
                Ok(())
            }
            packet_option::PACKET_VERSION => {
                let v = Self::parse_i32_opt(val)?;
                if v != TPACKET_V1 && v != TPACKET_V2 && v != TPACKET_V3 {
                    return Err(SystemError::EINVAL);
                }
                opts.version = v;
                Ok(())
            }
            packet_option::PACKET_RESERVE => {
                opts.reserve = Self::parse_u32_opt(val)?;
                Ok(())
            }
            packet_option::PACKET_VNET_HDR => {
                opts.vnet_hdr = Self::parse_i32_opt(val)? != 0;
                Ok(())
            }
            packet_option::PACKET_TX_TIMESTAMP => {
                opts.tx_timestamp = Self::parse_i32_opt(val)?;
                Ok(())
            }
            packet_option::PACKET_TIMESTAMP => {
                opts.timestamp = Self::parse_i32_opt(val)?;
                Ok(())
            }
            packet_option::PACKET_QDISC_BYPASS => {
                opts.qdisc_bypass = Self::parse_i32_opt(val)? != 0;
                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
}
