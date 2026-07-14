//! AF_PACKET sockets.

mod binding;
mod ring;
mod rx;
mod sockopt;
mod tx;
mod uapi;

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::{mutex::Mutex, rwsem::RwSem, wait_queue::WaitQueue};
use crate::net::socket::common::EPollItems;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::{Socket, PMSG, PSOCK, PSOL};
use crate::process::cred::CAPFlags;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;

#[allow(unused_imports)]
pub use ring::{PacketFakeFs, PacketRing, RingWriteResult, TpacketVersion};
#[allow(unused_imports)]
pub use uapi::{
    eth_protocol, packet_mreq_type, packet_option, PacketMreq, PacketType, SockAddrLl,
    TpacketAuxdata,
};

const DEFAULT_RX_BUFFER_SIZE: usize = 256 * 1024;
const DEFAULT_TX_BUFFER_SIZE: usize = 256 * 1024;
const READ_SCRATCH_BUFFER_SIZE: usize = DEFAULT_RX_BUFFER_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketSocketType {
    Raw,
    Dgram,
}

#[derive(Debug, Clone, Default)]
pub struct PacketMetadata {
    pub src_mac: [u8; 6],
    #[allow(dead_code)]
    pub dst_mac: [u8; 6],
    pub protocol: u16,
    pub ifindex: u32,
    pub pkt_type: PacketType,
    pub wire_len: usize,
    #[allow(dead_code)]
    pub mac_offset: usize,
    pub net_offset: usize,
    pub vlan_tci: u16,
    pub vlan_tpid: u16,
}

#[derive(Debug, Clone)]
pub struct ReceivedPacket {
    pub data: alloc::vec::Vec<u8>,
    pub metadata: PacketMetadata,
    accounted_bytes: usize,
}

#[derive(Debug, Default)]
pub struct PacketSocketOptions {
    pub auxdata: bool,
}

#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct PacketSocket {
    pub(super) sock_type: PacketSocketType,
    pub(super) binding: binding::PacketBinding,
    pub(super) bind_lock: Mutex<()>,
    pub(super) bound_iface: RwSem<Option<Arc<dyn Iface>>>,
    pub(super) rx_buffer: Mutex<VecDeque<ReceivedPacket>>,
    pub(super) rx_buffer_bytes: AtomicUsize,
    pub(super) send_buffer_bytes: AtomicUsize,
    pub(super) recv_buffer_bytes: AtomicUsize,
    pub(super) options: RwSem<PacketSocketOptions>,
    pub(super) stats_packets: AtomicU32,
    pub(super) stats_drops: AtomicU32,
    pub(super) nonblock: AtomicBool,
    pub(super) send_timeout_ticks: AtomicU64,
    pub(super) recv_timeout_ticks: AtomicU64,
    pub(super) wait_queue: WaitQueue,
    inode_id: InodeId,
    open_files: AtomicUsize,
    pub(super) self_ref: Weak<Self>,
    pub(super) netns: Arc<NetNamespace>,
    epoll_items: EPollItems,
    fasync_items: FAsyncItems,
    pub(super) rx_ring: Mutex<Option<Arc<Mutex<ring::PacketRing>>>>,
    pub(super) tpacket_version: Mutex<ring::TpacketVersion>,
    pub(super) tp_reserve: AtomicU32,
}

impl PacketSocket {
    pub fn new(sock_type: PSOCK, protocol: u16, nonblock: bool) -> Result<Arc<Self>, SystemError> {
        if !ProcessManager::current_pcb()
            .cred()
            .has_capability(CAPFlags::CAP_NET_RAW)
        {
            return Err(SystemError::EPERM);
        }
        let sock_type = match sock_type {
            PSOCK::Raw => PacketSocketType::Raw,
            PSOCK::Datagram => PacketSocketType::Dgram,
            _ => return Err(SystemError::ESOCKTNOSUPPORT),
        };
        let netns = ProcessManager::current_netns();
        let socket = Arc::new_cyclic(|me| Self {
            sock_type,
            binding: binding::PacketBinding::new(0, protocol),
            bind_lock: Mutex::new(()),
            bound_iface: RwSem::new(None),
            rx_buffer: Mutex::new(VecDeque::new()),
            rx_buffer_bytes: AtomicUsize::new(0),
            send_buffer_bytes: AtomicUsize::new(DEFAULT_TX_BUFFER_SIZE),
            recv_buffer_bytes: AtomicUsize::new(DEFAULT_RX_BUFFER_SIZE),
            options: RwSem::new(PacketSocketOptions::default()),
            stats_packets: AtomicU32::new(0),
            stats_drops: AtomicU32::new(0),
            nonblock: AtomicBool::new(nonblock),
            send_timeout_ticks: AtomicU64::new(crate::net::socket::common::INFINITE_TIMEOUT_TICKS),
            recv_timeout_ticks: AtomicU64::new(crate::net::socket::common::INFINITE_TIMEOUT_TICKS),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            rx_ring: Mutex::new(None),
            tpacket_version: Mutex::new(ring::TpacketVersion::V1),
            tp_reserve: AtomicU32::new(0),
        });
        socket.netns.register_packet_socket(socket.self_ref.clone());
        Ok(socket)
    }
    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }
    /// Returns the configured receive timeout in scheduler ticks, or None for infinite wait.
    pub(super) fn recv_timeout_ticks(&self) -> Option<u64> {
        let ticks = self.recv_timeout_ticks.load(Ordering::Relaxed);
        if ticks == crate::net::socket::common::INFINITE_TIMEOUT_TICKS {
            None
        } else {
            Some(ticks)
        }
    }
    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }
    pub fn self_ref(&self) -> Weak<Self> {
        self.self_ref.clone()
    }
}

/// Classify an Ethernet frame once at the AF_PACKET ingress boundary.
pub fn classify_packet(frame: &[u8], iface: &Arc<dyn Iface>) -> PacketType {
    if frame.len() < 6 {
        return PacketType::Host;
    }
    let dst = &frame[..6];
    if dst == [0xff; 6] {
        PacketType::Broadcast
    } else if dst[0] & 1 != 0 {
        PacketType::Multicast
    } else if dst == iface.mac().as_bytes() {
        PacketType::Host
    } else {
        PacketType::OtherHost
    }
}

/// Common AF_PACKET tap for Ethernet netdevices.
pub fn deliver_to_packet_sockets(iface: &Arc<dyn Iface>, frame: &[u8], pkt_type: PacketType) {
    if let Some(netns) = iface.net_namespace() {
        netns.deliver_to_packet_sockets(iface.nic_id() as u32, frame, pkt_type);
    }
}

pub fn packet_sockets_active(iface: &Arc<dyn Iface>) -> bool {
    iface
        .net_namespace()
        .is_some_and(|netns| netns.has_packet_sockets())
}

/// DragonOS loopback uses smoltcp's IP medium. Linux AF_PACKET exposes a
/// link-layer view, so synthesize the minimal loopback Ethernet header here.
pub fn deliver_ip_to_packet_sockets(iface: &Arc<dyn Iface>, packet: &[u8], pkt_type: PacketType) {
    let protocol = match packet.first().map(|byte| byte >> 4) {
        Some(4) => eth_protocol::ETH_P_IP,
        Some(6) => eth_protocol::ETH_P_IPV6,
        _ => return,
    };
    let mut frame = Vec::new();
    if frame.try_reserve_exact(14 + packet.len()).is_err() {
        return;
    }
    frame.resize(12, 0);
    frame.extend_from_slice(&protocol.to_be_bytes());
    frame.extend_from_slice(packet);
    deliver_to_packet_sockets(iface, &frame, pkt_type);
}

impl Socket for PacketSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }
    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }
    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        self.bind_endpoint(endpoint)
    }
    fn send_buffer_size(&self) -> usize {
        self.send_buffer_bytes.load(Ordering::Relaxed)
    }
    fn recv_buffer_size(&self) -> usize {
        self.recv_buffer_bytes.load(Ordering::Relaxed)
    }
    fn connect(&self, _: Endpoint) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
    fn send(&self, b: &[u8], f: PMSG) -> Result<usize, SystemError> {
        self.send_packet(b, f, None)
    }
    fn send_to(&self, b: &[u8], f: PMSG, a: Endpoint) -> Result<usize, SystemError> {
        self.send_endpoint(b, f, a)
    }
    fn validate_send_buffer_len(&self, l: usize, _: Option<&Endpoint>) -> Result<(), SystemError> {
        self.validate_packet_len(l)
    }
    fn recv(&self, b: &mut [u8], f: PMSG) -> Result<usize, SystemError> {
        self.recv_packet(b, f)
    }
    fn read_to_user_buffer(
        &self,
        b: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        crate::net::socket::base::read_to_user_buffer_via_kernel_buf(
            self,
            b,
            READ_SCRATCH_BUFFER_SIZE,
        )
    }
    fn recv_from(
        &self,
        b: &mut [u8],
        f: PMSG,
        _: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        self.recv_packet_from(b, f)
    }
    fn do_close(&self) -> Result<(), SystemError> {
        self.close_binding()
    }
    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        Err(SystemError::ENOTCONN)
    }
    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        self.packet_local_endpoint()
    }
    fn recv_msg(&self, m: &mut crate::net::posix::MsgHdr, f: PMSG) -> Result<usize, SystemError> {
        self.recv_packet_msg(m, f)
    }
    fn send_msg(&self, m: &crate::net::posix::MsgHdr, f: PMSG) -> Result<usize, SystemError> {
        self.send_packet_msg(m, f)
    }
    fn epoll_items(&self) -> &EPollItems {
        &self.epoll_items
    }
    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }
    fn check_io_event(&self) -> EPollEventType {
        self.packet_io_event()
    }
    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
    fn option(&self, l: PSOL, n: usize, v: &mut [u8]) -> Result<usize, SystemError> {
        self.packet_option(l, n, v)
    }
    fn set_option(&self, l: PSOL, n: usize, v: &[u8]) -> Result<(), SystemError> {
        self.set_packet_option(l, n, v)
    }
    fn mmap_layout(&self) -> Option<crate::net::socket::base::SocketMmapLayout> {
        let ring_outer = self.rx_ring.lock();
        ring_outer.as_ref().map(|r| {
            let inner = r.lock();
            crate::net::socket::base::SocketMmapLayout {
                page_cache: inner.page_cache().clone(),
                fs: Arc::new(ring::PacketFakeFs),
                size: inner.total_size(),
            }
        })
    }
}
