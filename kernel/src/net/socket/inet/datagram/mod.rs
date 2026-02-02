use inner::{UdpInner, UnboundUdp};
use smoltcp;
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::iov::IoVecs;
use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::mutex::Mutex;
use crate::libs::wait_queue::WaitQueue;
use crate::net::posix::{SockAddr, SockAddrIn};
use crate::net::socket::common::{EPollItems, ShutdownBit};
use crate::net::socket::unix::utils::CmsgBuffer;
use crate::net::socket::{AddressFamily, Socket, PMSG, PSO, PSOL};
use crate::net::socket::{IpOption, PIPV6};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;
use crate::{libs::rwsem::RwSem, net::socket::endpoint::Endpoint};
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use smoltcp::wire::{IpAddress::*, IpEndpoint, IpListenEndpoint, IpVersion};

use super::{InetSocket, UNSPECIFIED_LOCAL_ENDPOINT_V4, UNSPECIFIED_LOCAL_ENDPOINT_V6};

mod option;

pub mod inner;
pub mod multicast_loopback;
mod udp_bindings;

type EP = crate::filesystem::epoll::EPollEventType;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SockExtendedErr {
    ee_errno: u32,
    ee_origin: u8,
    ee_type: u8,
    ee_code: u8,
    ee_pad: u8,
    ee_info: u32,
    ee_data: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct InPktInfo {
    ipi_ifindex: i32,
    ipi_spec_dst: u32,
    ipi_addr: u32,
}

const SO_EE_ORIGIN_LOCAL: u8 = 1;
const ICMP_ECHOREPLY: u8 = 0;
const ICMP_NET_UNREACH: u8 = 0;

#[derive(Clone, Debug)]
struct UdpErrQueueEntry {
    err: SockExtendedErr,
    offender: IpEndpoint,
    cmsg_level: i32,
    cmsg_type: i32,
    addr_len: usize,
}

// Udp Socket 负责提供状态切换接口、执行状态切换
#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct UdpSocket {
    inner: RwSem<Option<UdpInner>>,
    nonblock: AtomicBool,
    shutdown: AtomicU8,
    wait_queue: WaitQueue,
    inode_id: InodeId,
    open_files: AtomicUsize,
    self_ref: Weak<UdpSocket>,
    netns: Arc<NetNamespace>,
    epoll_items: EPollItems,
    fasync_items: FAsyncItems,
    /// Custom send buffer size (SO_SNDBUF), 0 means use default
    send_buf_size: AtomicUsize,
    /// Custom receive buffer size (SO_RCVBUF), 0 means use default
    recv_buf_size: AtomicUsize,
    /// SO_RCVLOWAT
    rcvlowat: AtomicI32,
    /// SO_REUSEADDR
    so_reuseaddr: AtomicBool,
    /// SO_REUSEPORT
    so_reuseport: AtomicBool,
    /// SO_KEEPALIVE
    so_keepalive: AtomicBool,
    /// SO_BROADCAST
    so_broadcast: AtomicBool,
    /// SO_PASSCRED
    so_passcred: AtomicBool,
    /// IP_RECVTOS
    recv_tos: AtomicBool,
    /// IPV6_RECVTCLASS
    recv_tclass: AtomicBool,
    /// IP_RECVERR
    recv_err_v4: AtomicBool,
    /// IPV6_RECVERR
    recv_err_v6: AtomicBool,
    /// IP_MULTICAST_TTL (stored as int)
    ip_multicast_ttl: AtomicI32,
    /// IP_MULTICAST_LOOP
    ip_multicast_loop: AtomicBool,
    /// IP_MULTICAST_IF: interface index
    ip_multicast_ifindex: AtomicI32,
    /// IP_MULTICAST_IF: interface address (network byte order)
    ip_multicast_addr: AtomicU32,
    /// IP_ADD_MEMBERSHIP/IP_DROP_MEMBERSHIP state (best-effort, no actual IGMP)
    ip_multicast_groups: Mutex<Vec<crate::net::socket::inet::common::Ipv4MulticastMembership>>,
    /// IP_PKTINFO
    recv_pktinfo_v4: AtomicBool,
    /// IP_RECVORIGDSTADDR (aka IP_ORIGDSTADDR)
    recv_origdstaddr_v4: AtomicBool,
    /// IPV6_RECVORIGDSTADDR
    recv_origdstaddr_v6: AtomicBool,
    /// Error queue for MSG_ERRQUEUE
    errqueue: Mutex<VecDeque<UdpErrQueueEntry>>,
    /// SO_LINGER
    linger_onoff: AtomicI32,
    linger_linger: AtomicI32,
    /// SO_SNDTIMEO (microseconds). u64::MAX means "no timeout".
    send_timeout_us: AtomicU64,
    /// SO_RCVTIMEO (microseconds). u64::MAX means "no timeout".
    recv_timeout_us: AtomicU64,
    /// SO_NO_CHECK: disable UDP checksum (0=off, 1=on)
    ///
    /// NOTE: This is currently a stub implementation. The value can be set/get via
    /// setsockopt/getsockopt, but does NOT actually control UDP checksum behavior.
    ///
    /// Reason: smoltcp 0.12.0 does not support per-socket checksum control. Checksum
    /// behavior is controlled globally by DeviceCapabilities.checksum, which is set at
    /// the Device/Interface level, not per-socket.
    ///
    /// To implement this properly would require either:
    /// 1. Smoltcp feature that supports per-socket checksum control
    /// 2. Patching smoltcp to add this feature
    /// 3. Manually parsing/building UDP packets to bypass smoltcp's checksum handling
    no_check: AtomicBool,
    ip_version: IpVersion,
    /// Queue for multicast loopback packets
    /// This is separate from smoltcp's rx buffer because smoltcp doesn't support
    /// multicast loopback delivery across different interface socket sets
    multicast_loopback_rx: Mutex<VecDeque<LoopbackPacket>>,
}

/// A packet received via loopback delivery (multicast/unicast)
#[derive(Clone, Debug)]
struct LoopbackPacket {
    src_endpoint: IpEndpoint,
    dst_addr: smoltcp::wire::IpAddress,
    dst_port: u16,
    ifindex: i32,
    payload: Vec<u8>,
}

impl UdpSocket {
    pub fn new(nonblock: bool, version: IpVersion) -> Arc<Self> {
        let netns = ProcessManager::current_netns();
        Arc::new_cyclic(|me| Self {
            inner: RwSem::new(Some(UdpInner::Unbound(UnboundUdp::new()))),
            nonblock: AtomicBool::new(nonblock),
            shutdown: AtomicU8::new(0),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            send_buf_size: AtomicUsize::new(0), // 0 means use default
            recv_buf_size: AtomicUsize::new(0), // 0 means use default
            rcvlowat: AtomicI32::new(1),
            so_reuseaddr: AtomicBool::new(false),
            so_reuseport: AtomicBool::new(false),
            so_keepalive: AtomicBool::new(false),
            so_broadcast: AtomicBool::new(false),
            so_passcred: AtomicBool::new(false),
            recv_tos: AtomicBool::new(false),
            recv_tclass: AtomicBool::new(false),
            recv_err_v4: AtomicBool::new(false),
            recv_err_v6: AtomicBool::new(false),
            ip_multicast_ttl: AtomicI32::new(1),
            ip_multicast_loop: AtomicBool::new(true),
            ip_multicast_ifindex: AtomicI32::new(0),
            ip_multicast_addr: AtomicU32::new(0),
            ip_multicast_groups: Mutex::new(Vec::new()),
            recv_pktinfo_v4: AtomicBool::new(false),
            recv_origdstaddr_v4: AtomicBool::new(false),
            recv_origdstaddr_v6: AtomicBool::new(false),
            errqueue: Mutex::new(VecDeque::new()),
            linger_onoff: AtomicI32::new(0),
            linger_linger: AtomicI32::new(0),
            send_timeout_us: AtomicU64::new(u64::MAX),
            recv_timeout_us: AtomicU64::new(u64::MAX),
            no_check: AtomicBool::new(false), // checksums enabled by default
            ip_version: version,
            multicast_loopback_rx: Mutex::new(VecDeque::new()),
        })
    }

    #[inline]
    fn bind_id(&self) -> usize {
        self as *const UdpSocket as usize
    }

    #[inline]
    fn unspecified_addr(&self) -> smoltcp::wire::IpAddress {
        match self.ip_version {
            IpVersion::Ipv4 => smoltcp::wire::IpAddress::v4(0, 0, 0, 0),
            IpVersion::Ipv6 => smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0),
        }
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn recv_timeout(&self) -> Option<crate::time::Duration> {
        let us = self
            .recv_timeout_us
            .load(core::sync::atomic::Ordering::Relaxed);
        if us == u64::MAX {
            None
        } else {
            Some(crate::time::Duration::from_micros(us))
        }
    }

    fn loopback_accepts_with_preconnect(
        &self,
        pkt: &LoopbackPacket,
        consume_preconnect: bool,
    ) -> bool {
        let inner = self.inner.read();
        let bound = match inner.as_ref() {
            Some(UdpInner::Bound(bound)) => bound,
            _ => return false,
        };
        let local = bound.endpoint();
        if local.port != pkt.dst_port {
            return false;
        }
        if let Some(addr) = local.addr {
            if addr != pkt.dst_addr {
                if pkt.dst_addr.is_multicast() || pkt.dst_addr.is_broadcast() {
                    return false;
                }
                if addr.is_multicast() || addr.is_broadcast() {
                    return false;
                }
                return false;
            }
        }
        if let Ok(remote) = bound.remote_endpoint() {
            if remote != pkt.src_endpoint {
                let allow = if consume_preconnect {
                    bound.take_preconnect_data()
                } else {
                    bound.has_preconnect_data()
                };
                if !allow {
                    return false;
                }
            }
        }
        true
    }

    fn try_recv_loopback(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Option<(usize, IpEndpoint, usize, smoltcp::wire::IpAddress, i32)> {
        let mut loopback_rx = self.multicast_loopback_rx.lock();
        while let Some(pkt) = loopback_rx.pop_front() {
            if !self.loopback_accepts_with_preconnect(&pkt, !peek) {
                continue;
            }
            let copy_len = core::cmp::min(buf.len(), pkt.payload.len());
            buf[..copy_len].copy_from_slice(&pkt.payload[..copy_len]);
            let orig_len = pkt.payload.len();
            let src = pkt.src_endpoint;
            let dst = pkt.dst_addr;
            let ifindex = pkt.ifindex;
            if peek {
                loopback_rx.push_front(pkt);
            }
            return Some((copy_len, src, orig_len, dst, ifindex));
        }
        None
    }

    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut inner = self.inner.write();

        // Check socket state first without taking
        match inner.as_ref() {
            None => return Err(SystemError::EBADF),
            Some(UdpInner::Bound(_)) => return Err(SystemError::EINVAL), // Already bound
            Some(UdpInner::Unbound(_)) => {}
        }

        // Now safe to take - we know it's Unbound
        let _old_unbound = match inner.take() {
            Some(UdpInner::Unbound(unbound)) => unbound,
            _ => unreachable!(),
        };

        // Check if custom buffer sizes have been set via setsockopt
        let rx_size = self.recv_buf_size.load(Ordering::Acquire);
        let tx_size = self.send_buf_size.load(Ordering::Acquire);

        // log::debug!(
        //     "do_bind: rx_size={}, tx_size={}, will use custom buffers={}",
        //     rx_size,
        //     tx_size,
        //     rx_size > 0 || tx_size > 0
        // );

        // Create new UnboundUdp with custom buffer sizes if they've been set
        let unbound = if rx_size > 0 || tx_size > 0 {
            // log::debug!(
            //     "do_bind: creating socket with custom buffer sizes rx={}, tx={}",
            //     rx_size,
            //     tx_size
            // );
            UnboundUdp::new_with_buf_size(rx_size, tx_size)
        } else {
            // log::debug!("do_bind: creating socket with default buffer sizes");
            UnboundUdp::new()
        };

        let reuseaddr = self.so_reuseaddr.load(Ordering::Relaxed);
        let reuseport = self.so_reuseport.load(Ordering::Relaxed);
        match unbound.bind(
            local_endpoint,
            self.netns(),
            reuseaddr,
            reuseport,
            self.bind_id(),
        ) {
            Ok(bound) => {
                bound
                    .inner()
                    .iface()
                    .common()
                    .bind_socket(self.self_ref.upgrade().unwrap());
                let local = bound.endpoint();
                let addr = local.addr.unwrap_or_else(|| self.unspecified_addr());
                udp_bindings::register_udp_binding(
                    &self.netns,
                    self.self_ref.clone(),
                    addr,
                    local.port,
                    reuseaddr,
                    reuseport,
                );
                *inner = Some(UdpInner::Bound(bound));
                Ok(())
            }
            Err(e) => {
                // Restore unbound state on error
                *inner = Some(UdpInner::Unbound(UnboundUdp::new()));
                Err(e)
            }
        }
    }

    pub fn bind_ephemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        let mut inner_guard = self.inner.write();
        let inner = inner_guard.take().ok_or(SystemError::EBADF)?;
        let mut newly_bound_iface = None;
        let bound = match inner {
            UdpInner::Bound(inner) => inner,
            UdpInner::Unbound(_old_inner) => {
                // Check if custom buffer sizes have been set via setsockopt
                let rx_size = self.recv_buf_size.load(Ordering::Acquire);
                let tx_size = self.send_buf_size.load(Ordering::Acquire);

                // Create new UnboundUdp with custom buffer sizes if they've been set
                let inner = if rx_size > 0 || tx_size > 0 {
                    UnboundUdp::new_with_buf_size(rx_size, tx_size)
                } else {
                    UnboundUdp::new()
                };

                let reuseaddr = self.so_reuseaddr.load(Ordering::Relaxed);
                let reuseport = self.so_reuseport.load(Ordering::Relaxed);
                match inner.bind_ephemeral(
                    remote,
                    self.netns(),
                    reuseaddr,
                    reuseport,
                    self.bind_id(),
                ) {
                    Ok(bound) => {
                        newly_bound_iface = Some(bound.inner().iface().clone());
                        let local = bound.endpoint();
                        let addr = local.addr.unwrap_or_else(|| self.unspecified_addr());
                        udp_bindings::register_udp_binding(
                            &self.netns,
                            self.self_ref.clone(),
                            addr,
                            local.port,
                            reuseaddr,
                            reuseport,
                        );
                        bound
                    }
                    Err(e) => {
                        inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                        return Err(e);
                    }
                }
            }
        };
        // IMPORTANT: register this socket for iface notifications when it becomes bound implicitly.
        // Without this, incoming packets may not wake recv()/poll waiters, causing hangs in
        // gVisor tests such as UdpSocketTest.ReceiveAfterDisconnect.
        if let Some(iface) = newly_bound_iface {
            iface.common().bind_socket(self.self_ref.upgrade().unwrap());
        }
        inner_guard.replace(UdpInner::Bound(bound));
        Ok(())
    }

    pub fn is_bound(&self) -> bool {
        let inner = self.inner.read();
        if let Some(UdpInner::Bound(_)) = &*inner {
            return true;
        }
        return false;
    }

    /// Recreates the socket with new buffer sizes if it's already bound.
    /// This is needed because smoltcp doesn't support resizing socket buffers dynamically.
    fn recreate_socket_if_bound(&self) -> Result<(), SystemError> {
        let mut inner_guard = self.inner.write();

        // Check if socket is bound
        let bound = match inner_guard.as_ref() {
            Some(UdpInner::Bound(b)) => b,
            _ => return Ok(()), // Not bound, nothing to do
        };

        // Save current state before recreating
        let local_ep = bound.endpoint();
        let remote_ep = bound.remote_endpoint().ok(); // May be None if not connected
        let _explicitly_bound = !bound.should_unbind_on_disconnect();

        // log::debug!(
        //     "Recreating UDP socket: local={:?}, remote={:?}, explicit={}",
        //     local_ep,
        //     remote_ep,
        //     explicitly_bound
        // );

        // Get the local address and port
        let IpListenEndpoint { addr, port } = local_ep;
        let local_addr = addr.unwrap_or_else(|| smoltcp::wire::IpAddress::v4(0, 0, 0, 0));

        // Unbind the old socket and drop it
        if let Some(UdpInner::Bound(b)) = inner_guard.take() {
            udp_bindings::unregister_udp_binding(&self.netns, &self.self_ref);
            b.close();
        }

        // Create new UnboundUdp with new buffer sizes
        let rx_size = self.recv_buf_size.load(Ordering::Acquire);
        let tx_size = self.send_buf_size.load(Ordering::Acquire);
        let unbound = if rx_size > 0 || tx_size > 0 {
            UnboundUdp::new_with_buf_size(rx_size, tx_size)
        } else {
            UnboundUdp::new()
        };

        // Rebind to the same endpoint
        let new_endpoint = smoltcp::wire::IpEndpoint::new(local_addr, port);
        let reuseaddr = self.so_reuseaddr.load(Ordering::Relaxed);
        let reuseport = self.so_reuseport.load(Ordering::Relaxed);
        let bound = match unbound.bind(
            new_endpoint,
            self.netns(),
            reuseaddr,
            reuseport,
            self.bind_id(),
        ) {
            Ok(b) => b,
            Err(e) => {
                // Restore unbound state on error
                *inner_guard = Some(UdpInner::Unbound(UnboundUdp::new()));
                return Err(e);
            }
        };

        // Restore connection if it existed
        if let Some(remote) = remote_ep {
            bound.connect(remote);
        }

        // Restore the binding in the interface
        bound
            .inner()
            .iface()
            .common()
            .bind_socket(self.self_ref.upgrade().unwrap());
        udp_bindings::register_udp_binding(
            &self.netns,
            self.self_ref.clone(),
            local_addr,
            port,
            reuseaddr,
            reuseport,
        );

        *inner_guard = Some(UdpInner::Bound(bound));

        Ok(())
    }

    pub fn close(&self) {
        let mut inner = self.inner.write();
        if let Some(UdpInner::Bound(bound)) = &mut *inner {
            udp_bindings::unregister_udp_binding(&self.netns, &self.self_ref);
            multicast_loopback::multicast_registry().unregister_all(&self.self_ref);
            crate::net::socket::inet::common::multicast::drop_ipv4_memberships(
                &self.netns,
                &self.ip_multicast_groups,
            );
            bound
                .inner()
                .iface()
                .common()
                .unbind_socket(self.self_ref.upgrade().unwrap());
            bound.close();
            inner.take();
        }
        // unbound socket just drop (only need to free memory)
    }

    pub fn try_recv(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<(usize, smoltcp::wire::IpEndpoint, usize), SystemError> {
        // First, check loopback queue (multicast/unicast)
        if let Some((copy_len, endpoint, orig_len, _dst, _ifindex)) =
            self.try_recv_loopback(buf, peek)
        {
            return Ok((copy_len, endpoint, orig_len));
        }

        // Then check smoltcp socket
        match self.inner.read().as_ref().ok_or(SystemError::EBADF)? {
            UdpInner::Bound(bound) => bound.try_recv(buf, peek),
            // UDP is connectionless - unbound socket just has no data yet
            UdpInner::Unbound(_) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
        }
    }

    pub fn try_recv_with_meta(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<
        (
            usize,
            smoltcp::wire::IpEndpoint,
            usize,
            smoltcp::wire::IpAddress,
            i32,
        ),
        SystemError,
    > {
        if let Some((copy_len, endpoint, orig_len, dst_addr, ifindex)) =
            self.try_recv_loopback(buf, peek)
        {
            return Ok((copy_len, endpoint, orig_len, dst_addr, ifindex));
        }

        let inner = self.inner.read();
        let bound = match inner.as_ref() {
            Some(UdpInner::Bound(bound)) => bound,
            _ => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
        };
        let ifindex = bound.inner().iface().nic_id() as i32;
        let (copy_len, endpoint, orig_len, dst_addr) = bound.try_recv_with_metadata(buf, peek)?;
        let dst_addr = dst_addr.unwrap_or_else(|| self.unspecified_addr());
        Ok((copy_len, endpoint, orig_len, dst_addr, ifindex))
    }

    fn local_port(&self) -> Option<u16> {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => Some(bound.endpoint().port),
            _ => None,
        }
    }

    fn build_udp_recv_cmsgs(
        &self,
        cmsg_buf: &mut CmsgBuffer,
        msg_flags: &mut i32,
        dst_addr: smoltcp::wire::IpAddress,
        ifindex: i32,
    ) -> Result<(), SystemError> {
        if self.ip_version != IpVersion::Ipv4 {
            return Ok(());
        }
        let (v4, local_port) = match (dst_addr, self.local_port()) {
            (smoltcp::wire::IpAddress::Ipv4(v4), Some(port)) => (v4, port),
            _ => return Ok(()),
        };
        let dst = v4.to_bits().to_be();
        let spec_dst = crate::net::socket::inet::common::multicast::find_iface_by_ifindex(
            &self.netns,
            ifindex,
        )
        .and_then(|iface| iface.common().ipv4_addr())
        .map(|addr| addr.to_bits().to_be())
        .unwrap_or(dst);

        if self.recv_pktinfo_v4.load(Ordering::Relaxed) {
            let pktinfo = InPktInfo {
                ipi_ifindex: ifindex,
                ipi_spec_dst: spec_dst,
                ipi_addr: dst,
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&pktinfo as *const InPktInfo) as *const u8,
                    core::mem::size_of::<InPktInfo>(),
                )
            };
            cmsg_buf.put(
                msg_flags,
                PSOL::IP as i32,
                IpOption::PKTINFO as i32,
                core::mem::size_of::<InPktInfo>(),
                bytes,
            )?;
        }

        if self.recv_origdstaddr_v4.load(Ordering::Relaxed) {
            let sockaddr = SockAddrIn {
                sin_family: AddressFamily::INet as u16,
                sin_port: local_port.to_be(),
                sin_addr: dst,
                sin_zero: [0; 8],
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&sockaddr as *const SockAddrIn) as *const u8,
                    core::mem::size_of::<SockAddrIn>(),
                )
            };
            cmsg_buf.put(
                msg_flags,
                PSOL::IP as i32,
                IpOption::ORIGDSTADDR as i32,
                core::mem::size_of::<SockAddrIn>(),
                bytes,
            )?;
        }
        Ok(())
    }

    #[inline]
    pub fn can_recv(&self) -> bool {
        // Can receive if there's data in multicast loopback queue or smoltcp queue
        // OR if read is shutdown (shutdown should wake up recv() to return 0/EOF)
        if !self.multicast_loopback_rx.lock().is_empty() {
            return true;
        }
        let has_data = self.check_io_event().contains(EP::EPOLLIN);
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let read_shutdown = (shutdown_bits & 0x01) != 0;
        has_data || read_shutdown
    }

    #[inline]
    #[allow(dead_code)]
    pub fn can_send(&self) -> bool {
        // Can send if socket is ready OR if write is shutdown
        // (shutdown should wake up send() to return EPIPE)
        let can_write = self.check_io_event().contains(EP::EPOLLOUT);
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let write_shutdown = (shutdown_bits & 0x02) != 0;
        can_write || write_shutdown
    }

    #[inline]
    fn recv_return_len(copy_len: usize, orig_len: usize, flags: PMSG) -> usize {
        if flags.contains(PMSG::TRUNC) {
            orig_len
        } else {
            copy_len
        }
    }

    fn enqueue_errqueue(
        &self,
        err: SockExtendedErr,
        offender: IpEndpoint,
        cmsg_level: i32,
        cmsg_type: i32,
        addr_len: usize,
    ) {
        let mut q = self.errqueue.lock();
        q.push_back(UdpErrQueueEntry {
            err,
            offender,
            cmsg_level,
            cmsg_type,
            addr_len,
        });
    }

    fn pop_errqueue(&self) -> Option<UdpErrQueueEntry> {
        self.errqueue.lock().pop_front()
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        // sendto(2) 目标端口为 0 应返回 EINVAL。
        if let Some(dest) = to {
            if dest.port == 0 {
                return Err(SystemError::EINVAL);
            }
        }

        // Send data and get iface reference, then release lock before polling
        let (
            result,
            send_iface,
            dest,
            loopback_send,
            send_iface_is_loopback,
            mcast_ifindex,
            restore_iface,
        ) = {
            let mut inner_guard = self.inner.write();

            // Check if socket is closed
            let inner = inner_guard.as_ref().ok_or(SystemError::EBADF)?;

            // If unbound, bind to ephemeral port
            if let UdpInner::Unbound(_) = inner {
                let to_addr = to.ok_or(SystemError::EDESTADDRREQ)?.addr;
                let unbound = match inner_guard.take().unwrap() {
                    UdpInner::Unbound(unbound) => unbound,
                    _ => unreachable!(),
                };
                let reuseaddr = self.so_reuseaddr.load(Ordering::Relaxed);
                let reuseport = self.so_reuseport.load(Ordering::Relaxed);
                match unbound.bind_ephemeral(
                    to_addr,
                    self.netns(),
                    reuseaddr,
                    reuseport,
                    self.bind_id(),
                ) {
                    Ok(bound) => {
                        // Register for iface notifications on implicit bind via sendto().
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                        let local = bound.endpoint();
                        let addr = local.addr.unwrap_or_else(|| self.unspecified_addr());
                        udp_bindings::register_udp_binding(
                            &self.netns,
                            self.self_ref.clone(),
                            addr,
                            local.port,
                            reuseaddr,
                            reuseport,
                        );
                        inner_guard.replace(UdpInner::Bound(bound));
                    }
                    Err(e) => {
                        // Restore unbound state on error
                        inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                        return Err(e);
                    }
                }
            }

            // Send data and get iface Arc before releasing lock
            match inner_guard.as_mut().ok_or(SystemError::EBADF)? {
                UdpInner::Bound(bound) => {
                    let dest = to
                        .or_else(|| bound.remote_endpoint().ok())
                        .ok_or(SystemError::EDESTADDRREQ)?;
                    let bound_iface = bound.inner().iface().clone();
                    let is_multicast = dest.addr.is_multicast();
                    let mcast_ifindex = if is_multicast {
                        let ifindex = self.ip_multicast_ifindex.load(Ordering::Relaxed);
                        if ifindex != 0 {
                            ifindex
                        } else {
                            bound_iface.nic_id() as i32
                        }
                    } else {
                        0
                    };
                    let send_iface_is_loopback = if mcast_ifindex != 0 {
                        crate::net::socket::inet::common::multicast::find_iface_by_ifindex(
                            &self.netns,
                            mcast_ifindex,
                        )
                        .and_then(|i| {
                            self.netns
                                .loopback_iface()
                                .map(|lo| lo.nic_id() == i.nic_id())
                        })
                        .unwrap_or(false)
                    } else {
                        self.netns
                            .loopback_iface()
                            .map(|lo| lo.nic_id() == bound_iface.nic_id())
                            .unwrap_or(false)
                    };
                    let is_loopback = match dest.addr {
                        Ipv4(v4) => v4.is_loopback(),
                        Ipv6(v6) => v6.is_loopback(),
                    };
                    let is_broadcast = matches!(dest.addr, Ipv4(v4) if v4.is_broadcast());
                    if is_loopback {
                        let max_payload =
                            bound.with_socket(|socket| socket.payload_send_capacity());
                        if buf.len() > max_payload || buf.len() > u16::MAX as usize {
                            (
                                Err(SystemError::EMSGSIZE),
                                bound_iface,
                                Some(dest),
                                true,
                                send_iface_is_loopback,
                                mcast_ifindex,
                                None,
                            )
                        } else {
                            (
                                Ok(buf.len()),
                                bound_iface,
                                Some(dest),
                                true,
                                send_iface_is_loopback,
                                mcast_ifindex,
                                None,
                            )
                        }
                    } else if (is_multicast || is_broadcast) && send_iface_is_loopback {
                        let max_payload =
                            bound.with_socket(|socket| socket.payload_send_capacity());
                        if buf.len() > max_payload || buf.len() > u16::MAX as usize {
                            (
                                Err(SystemError::EMSGSIZE),
                                bound_iface,
                                Some(dest),
                                true,
                                send_iface_is_loopback,
                                mcast_ifindex,
                                None,
                            )
                        } else {
                            (
                                Ok(buf.len()),
                                bound_iface,
                                Some(dest),
                                true,
                                send_iface_is_loopback,
                                mcast_ifindex,
                                None,
                            )
                        }
                    } else {
                        let mut send_iface = bound_iface.clone();
                        let mut restore_iface = None;
                        if is_multicast && mcast_ifindex != 0 {
                            if let Some(target_iface) =
                                crate::net::socket::inet::common::multicast::find_iface_by_ifindex(
                                    &self.netns,
                                    mcast_ifindex,
                                )
                            {
                                if !Arc::ptr_eq(&target_iface, &bound_iface) {
                                    restore_iface = Some(bound_iface.clone());
                                    bound.inner_mut().move_udp_to_iface(target_iface.clone())?;
                                    send_iface = target_iface;
                                }
                            }
                        }

                        let ret = bound.try_send(buf, to);
                        (
                            ret,
                            send_iface,
                            Some(dest),
                            false,
                            send_iface_is_loopback,
                            mcast_ifindex,
                            restore_iface,
                        )
                    }
                }
                _ => return Err(SystemError::ENOTCONN),
            }
        }; // Lock released here

        if loopback_send {
            if let Some(dest) = dest {
                let src_endpoint = match self.inner.read().as_ref() {
                    Some(UdpInner::Bound(bound)) => {
                        let local = bound.endpoint();
                        let local_addr = local.addr.unwrap_or_else(|| self.unspecified_addr());
                        IpEndpoint::new(local_addr, local.port)
                    }
                    _ => IpEndpoint::new(self.unspecified_addr(), 0),
                };
                let ifindex = self
                    .netns
                    .loopback_iface()
                    .map(|lo| lo.nic_id() as i32)
                    .unwrap_or_else(|| send_iface.nic_id() as i32);
                if dest.addr.is_multicast() {
                    if let Ipv4(addr) = dest.addr {
                        let octets = addr.octets();
                        let multiaddr = u32::from_ne_bytes(octets);
                        let ifindex = mcast_ifindex.max(ifindex);
                        if multicast_loopback::multicast_registry()
                            .has_membership(multiaddr, ifindex)
                        {
                            udp_bindings::deliver_multicast_all(
                                &self.netns,
                                dest,
                                src_endpoint,
                                ifindex,
                                buf,
                            );
                        }
                    }
                } else {
                    udp_bindings::deliver_unicast_loopback(
                        &self.netns,
                        dest,
                        src_endpoint,
                        ifindex,
                        buf,
                    );
                }

                // 为 raw socket 构建完整 IP 包并投递（用于 RAW 接收场景）。
                crate::net::socket::inet::raw::deliver_udp_loopback_packet(
                    &self.netns,
                    self.ip_version,
                    src_endpoint.addr,
                    dest.addr,
                    src_endpoint.port,
                    dest.port,
                    buf,
                );
            }
            if let Err(SystemError::EMSGSIZE) = result {
                if self.ip_version == IpVersion::Ipv6 && self.recv_err_v6.load(Ordering::Acquire) {
                    let mut off =
                        dest.unwrap_or_else(|| IpEndpoint::new(self.unspecified_addr(), 0));
                    if off.addr.is_unspecified() {
                        off.addr = smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1);
                    }
                    let mut ee = SystemError::EMSGSIZE.to_posix_errno();
                    if ee < 0 {
                        ee = -ee;
                    }
                    let err = SockExtendedErr {
                        ee_errno: ee as u32,
                        ee_origin: SO_EE_ORIGIN_LOCAL,
                        ee_type: ICMP_ECHOREPLY,
                        ee_code: ICMP_NET_UNREACH,
                        ee_pad: 0,
                        ee_info: buf.len() as u32,
                        ee_data: 0,
                    };
                    let addr_len = SockAddr::from(Endpoint::Ip(off)).len().unwrap_or(0) as usize;
                    if addr_len != 0 {
                        self.enqueue_errqueue(
                            err,
                            off,
                            PSOL::IPV6 as i32,
                            PIPV6::RECVERR as i32,
                            addr_len,
                        );
                    }
                }
            }
            return result;
        }

        // Poll AFTER releasing the lock to avoid deadlock
        // when socket sends to itself on loopback
        send_iface.poll();

        if let Some(orig_iface) = restore_iface {
            let mut inner_guard = self.inner.write();
            if let Some(UdpInner::Bound(bound)) = inner_guard.as_mut() {
                let _ = bound.inner_mut().move_udp_to_iface(orig_iface);
            }
        }

        // Multicast loopback: if sending to a multicast address and loopback is enabled,
        // deliver the packet to all local sockets that have joined the group
        if result.is_ok() {
            if let Some(dest) = dest.or_else(|| match self.inner.read().as_ref() {
                Some(UdpInner::Bound(bound)) => bound.remote_endpoint().ok(),
                _ => None,
            }) {
                let allow_mcast_loop =
                    self.is_multicast_loopback_enabled() || send_iface_is_loopback;
                if dest.addr.is_multicast() && allow_mcast_loop {
                    // Get the source endpoint (this socket's local address)
                    let src_endpoint = match self.inner.read().as_ref() {
                        Some(UdpInner::Bound(bound)) => {
                            let local = bound.endpoint();
                            let local_addr = local.addr.unwrap_or_else(|| self.unspecified_addr());
                            IpEndpoint::new(local_addr, local.port)
                        }
                        _ => IpEndpoint::new(self.unspecified_addr(), 0),
                    };

                    // Get multicast address and interface index
                    if let Ipv4(addr) = dest.addr {
                        let octets = addr.octets();
                        let multiaddr = u32::from_ne_bytes(octets);
                        let ifindex = self.get_multicast_ifindex();

                        if multicast_loopback::multicast_registry()
                            .has_membership(multiaddr, ifindex)
                        {
                            udp_bindings::deliver_multicast_all(
                                &self.netns,
                                dest,
                                src_endpoint,
                                ifindex,
                                buf,
                            );
                        }
                    }
                }
            }
        }

        if let Err(SystemError::EMSGSIZE) = result {
            if self.ip_version == IpVersion::Ipv6 && self.recv_err_v6.load(Ordering::Acquire) {
                let mut off = to.unwrap_or_else(|| IpEndpoint::new(self.unspecified_addr(), 0));
                if off.addr.is_unspecified() {
                    off.addr = smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1);
                }
                let mut ee = SystemError::EMSGSIZE.to_posix_errno();
                if ee < 0 {
                    ee = -ee;
                }
                let err = SockExtendedErr {
                    ee_errno: ee as u32,
                    ee_origin: SO_EE_ORIGIN_LOCAL,
                    ee_type: ICMP_ECHOREPLY,
                    ee_code: ICMP_NET_UNREACH,
                    ee_pad: 0,
                    ee_info: buf.len() as u32,
                    ee_data: 0,
                };
                let addr_len = SockAddr::from(Endpoint::Ip(off)).len().unwrap_or(0) as usize;
                if addr_len != 0 {
                    self.enqueue_errqueue(
                        err,
                        off,
                        PSOL::IPV6 as i32,
                        PIPV6::RECVERR as i32,
                        addr_len,
                    );
                }
            }
        }

        result
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    /// Inject a loopback packet into this socket's receive buffer
    ///
    /// Returns true if the packet was successfully injected
    pub fn inject_loopback_packet(
        &self,
        src_endpoint: IpEndpoint,
        dst_addr: smoltcp::wire::IpAddress,
        dst_port: u16,
        ifindex: i32,
        payload: &[u8],
    ) -> bool {
        // Check if socket is bound
        {
            let inner = self.inner.read();
            if !matches!(inner.as_ref(), Some(UdpInner::Bound(_))) {
                return false;
            }
        }

        // Add to multicast loopback queue
        let packet = LoopbackPacket {
            src_endpoint,
            dst_addr,
            dst_port,
            ifindex,
            payload: payload.to_vec(),
        };
        self.multicast_loopback_rx.lock().push_back(packet);

        // Wake up any waiting receivers
        self.wait_queue.wakeup(None);
        let pollflag = self.check_io_event();
        let _ = EventPoll::wakeup_epoll(self.epoll_items().as_ref(), pollflag);

        true
    }

    /// Get the interface index this socket is bound to (for multicast send interface)
    pub fn get_multicast_ifindex(&self) -> i32 {
        // First check if IP_MULTICAST_IF was explicitly set
        let ifindex = self.ip_multicast_ifindex.load(Ordering::Relaxed);
        if ifindex != 0 {
            return ifindex;
        }

        // Otherwise, use the interface the socket is bound to
        let inner = self.inner.read();
        match inner.as_ref() {
            Some(UdpInner::Bound(bound)) => bound.inner().iface().nic_id() as i32,
            _ => 0,
        }
    }

    pub fn has_ipv4_multicast_membership(&self, multiaddr: u32, ifindex: i32) -> bool {
        if ifindex <= 0 {
            return false;
        }
        let groups = self.ip_multicast_groups.lock();
        groups
            .iter()
            .any(|g| g.multiaddr == multiaddr && g.ifindex == ifindex)
    }

    /// Check if multicast loopback is enabled for this socket
    pub fn is_multicast_loopback_enabled(&self) -> bool {
        self.ip_multicast_loop.load(Ordering::Relaxed)
    }
}

impl Socket for UdpSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblock
            .store(nonblocking, core::sync::atomic::Ordering::Relaxed);
    }

    fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        match local_endpoint {
            Endpoint::Ip(local_endpoint) => self.do_bind(local_endpoint),
            Endpoint::Unspecified => {
                // AF_UNSPEC on bind() is a no-op for AF_INET sockets (Linux compatibility)
                // See: https://github.com/torvalds/linux/commit/29c486df6a208432b370bd4be99ae1369ede28d8
                // log::debug!("UDP bind: AF_UNSPEC treated as no-op for compatibility");
                Ok(())
            }
            _ => Err(SystemError::EAFNOSUPPORT),
        }
    }

    fn send_buffer_size(&self) -> usize {
        // Check if custom buffer size was set via setsockopt
        let custom_size = self.send_buf_size.load(Ordering::Acquire);
        if custom_size > 0 {
            // Linux doubles the value when returning via getsockopt
            return custom_size * 2;
        }

        // Otherwise return actual buffer capacity
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity())
            }
            _ => inner::DEFAULT_TX_BUF_SIZE * 2, // Linux doubles default too
        }
    }

    fn recv_buffer_size(&self) -> usize {
        // Check if custom buffer size was set via setsockopt
        let custom_size = self.recv_buf_size.load(Ordering::Acquire);
        if custom_size > 0 {
            // Linux doubles the value when returning via getsockopt
            // log::debug!(
            //     "recv_buffer_size: custom_size={}, returning={}",
            //     custom_size,
            //     custom_size * 2
            // );
            return custom_size * 2;
        }

        // Otherwise return actual buffer capacity
        let size = match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_recv_capacity())
            }
            _ => inner::DEFAULT_RX_BUF_SIZE * 2, // Linux doubles default too
        };
        // log::debug!("recv_buffer_size: no custom size, returning={}", size);
        size
    }

    fn recv_bytes_available(&self) -> usize {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                // 优先检查 loopback 队列，返回第一条可接收报文的长度。
                let loopback_len = {
                    let loopback_rx = self.multicast_loopback_rx.lock();
                    loopback_rx
                        .iter()
                        .find(|pkt| self.loopback_accepts_with_preconnect(pkt, false))
                        .map(|pkt| pkt.payload.len())
                };
                if let Some(len) = loopback_len {
                    return len;
                }

                // For UDP, FIONREAD should return the size of the first packet,
                // not the total bytes in the queue
                bound.with_mut_socket(|socket| match socket.peek() {
                    Ok((payload, _)) => payload.len(),
                    Err(_) => 0, // No packets available
                })
            }
            _ => 0,
        }
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        match endpoint {
            Endpoint::Ip(remote) => {
                // Port 0 is treated as disconnect (like AF_UNSPEC)
                // This matches Linux behavior where connect() to port 0 succeeds but disconnects the socket
                if remote.port == 0 {
                    // log::debug!("UDP connect: port 0 treated as disconnect");
                    // Disconnect logic - same as AF_UNSPEC case
                    let should_unbind = {
                        match self.inner.read().as_ref() {
                            Some(UdpInner::Bound(inner)) => {
                                inner.disconnect();
                                inner.should_unbind_on_disconnect()
                            }
                            Some(UdpInner::Unbound(_)) => return Ok(()), // Already disconnected
                            None => return Err(SystemError::EBADF),
                        }
                    };

                    if should_unbind {
                        // Socket was implicitly bound by connect, unbind it
                        let mut inner_guard = self.inner.write();
                        if let Some(UdpInner::Bound(bound)) = inner_guard.take() {
                            udp_bindings::unregister_udp_binding(&self.netns, &self.self_ref);
                            multicast_loopback::multicast_registry().unregister_all(&self.self_ref);
                            bound
                                .inner()
                                .iface()
                                .common()
                                .unbind_socket(self.self_ref.upgrade().unwrap());
                            bound.close();
                            inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                        }
                    }
                    return Ok(());
                }

                if !self.is_bound() {
                    self.bind_ephemeral(remote.addr)?;
                }
                match self.inner.read().as_ref() {
                    Some(UdpInner::Bound(inner)) => {
                        inner.connect(remote);
                        if !self.multicast_loopback_rx.lock().is_empty() {
                            inner.set_preconnect_data(true);
                        }
                        Ok(())
                    }
                    Some(_) => Err(SystemError::ENOTCONN),
                    None => Err(SystemError::EBADF),
                }
            }
            Endpoint::Unspecified => {
                // AF_UNSPEC: disconnect the UDP socket (clear remote endpoint)
                // If socket was implicitly bound (by connect), unbind it
                let should_unbind = {
                    match self.inner.read().as_ref() {
                        Some(UdpInner::Bound(inner)) => {
                            inner.disconnect();
                            inner.should_unbind_on_disconnect()
                        }
                        Some(UdpInner::Unbound(_)) => return Ok(()), // Already disconnected
                        None => return Err(SystemError::EBADF),
                    }
                };

                if should_unbind {
                    // Socket was implicitly bound by connect, unbind it
                    let mut inner_guard = self.inner.write();
                    if let Some(UdpInner::Bound(bound)) = inner_guard.take() {
                        udp_bindings::unregister_udp_binding(&self.netns, &self.self_ref);
                        multicast_loopback::multicast_registry().unregister_all(&self.self_ref);
                        bound
                            .inner()
                            .iface()
                            .common()
                            .unbind_socket(self.self_ref.upgrade().unwrap());
                        bound.close();
                        inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                    }
                }
                Ok(())
            }
            _ => Err(SystemError::EAFNOSUPPORT),
        }
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if buffer.is_empty() {
            log::debug!("UDP send() called with ZERO-LENGTH buffer");
        }

        // Check if write is shutdown (0x02 = SEND_SHUTDOWN)
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        if shutdown_bits & 0x02 != 0 {
            return Err(SystemError::EPIPE);
        }

        if flags.contains(PMSG::DONTWAIT) {
            log::warn!("Nonblock send is not implemented yet");
        }

        return self.try_send(buffer, None);
    }

    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError> {
        // Check if write is shutdown (0x02 = SEND_SHUTDOWN)
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        if shutdown_bits & 0x02 != 0 {
            return Err(SystemError::EPIPE);
        }

        if flags.contains(PMSG::DONTWAIT) {
            log::warn!("Nonblock send is not implemented yet");
        }

        if let Endpoint::Ip(remote) = address {
            return self.try_send(buffer, Some(remote));
        }

        return Err(SystemError::EINVAL);
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        // Check if read is shutdown
        // Linux allows reading buffered data even after SHUT_RD, only returns EOF when buffer is empty
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let is_recv_shutdown = shutdown_bits & 0x01 != 0;

        let peek = flags.contains(PMSG::PEEK);

        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            let result = self.try_recv(buffer, peek);
            // If shutdown and no data available, return EOF instead of EWOULDBLOCK
            if is_recv_shutdown && matches!(result, Err(SystemError::EAGAIN_OR_EWOULDBLOCK)) {
                return Ok(0);
            }
            return result.map(|(copy_len, _endpoint, orig_len)| {
                Self::recv_return_len(copy_len, orig_len, flags)
            });
        } else {
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                match self.try_recv(buffer, peek) {
                    Ok((copy_len, _endpoint, orig_len)) => {
                        return Ok(Self::recv_return_len(copy_len, orig_len, flags));
                    }
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        // If shutdown and no data available, return EOF
                        if is_recv_shutdown {
                            return Ok(0);
                        }
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || self.can_recv(),
                            self.recv_timeout(),
                        )?;
                    }
                    Err(e) => return Err(e),
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
        // Linux allows reading buffered data even after SHUT_RD
        // For blocking mode, check shutdown state in the loop

        let peek = flags.contains(PMSG::PEEK);

        return if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            let result = self.try_recv(buffer, peek);
            // For non-blocking sockets, always return EAGAIN when no data
            // Even after shutdown, don't convert to EOF
            result.map(|(copy_len, endpoint, orig_len)| {
                (
                    Self::recv_return_len(copy_len, orig_len, flags),
                    Endpoint::Ip(endpoint),
                )
            })
        } else {
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                match self.try_recv(buffer, peek) {
                    Ok((copy_len, endpoint, orig_len)) => {
                        return Ok((
                            Self::recv_return_len(copy_len, orig_len, flags),
                            Endpoint::Ip(endpoint),
                        ));
                    }
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        // If shutdown and no data available, return EOF
                        if is_recv_shutdown {
                            // If connected, return EOF with remote endpoint
                            if let Some(UdpInner::Bound(bound)) = self.inner.read().as_ref() {
                                if let Ok(remote) = bound.remote_endpoint() {
                                    return Ok((0, Endpoint::Ip(remote)));
                                }
                            }
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || self.can_recv(),
                            self.recv_timeout(),
                        )?;
                        // log::debug!("UdpSocket::recv_from: wake up");
                    }
                    Err(e) => return Err(e),
                }
            }
        };
    }

    fn do_close(&self) -> Result<(), SystemError> {
        self.close();
        Ok(())
    }

    fn shutdown(&self, how: ShutdownBit) -> Result<(), SystemError> {
        // For UDP, shutdown requires the socket to be connected (both SHUT_RD and SHUT_WR)
        // Check if socket is connected
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                if bound.remote_endpoint().is_err() {
                    return Err(SystemError::ENOTCONN);
                }
            }
            Some(UdpInner::Unbound(_)) => {
                return Err(SystemError::ENOTCONN);
            }
            None => return Err(SystemError::EBADF),
        }

        // Set the shutdown bits atomically
        // Use fetch_or to set the bits we want
        let _old = self.shutdown.fetch_or(
            (if how.is_recv_shutdown() { 0x01 } else { 0 })
                | (if how.is_send_shutdown() { 0x02 } else { 0 }),
            Ordering::Release,
        );

        // log::debug!(
        //     "UDP shutdown: old={:#x}, recv={}, send={}",
        //     _old,
        //     how.is_recv_shutdown(),
        //     how.is_send_shutdown()
        // );

        // Wake up any threads blocked in recv() or send() so they can check the shutdown state
        self.wait_queue.wakeup_all(None);

        Ok(())
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_socket_option(opt, val)
            }
            PSOL::IP => {
                let opt = IpOption::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_ip_option(opt, val)
            }
            PSOL::IPV6 => {
                if self.ip_version != IpVersion::Ipv6 {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                let opt = PIPV6::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_ipv6_option(opt, val)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        // log::debug!(
        //     "UDP getsockopt called: level={:?}, name={}, value_len={}",
        //     level,
        //     name,
        //     value.len()
        // );
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_socket_option(opt, value)
            }
            PSOL::IP => {
                let opt = IpOption::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_ip_option(opt, value)
            }
            PSOL::IPV6 => {
                if self.ip_version != IpVersion::Ipv6 {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                let opt = PIPV6::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_ipv6_option(opt, value)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => Ok(Endpoint::Ip(bound.remote_endpoint()?)),
            Some(_) => Err(SystemError::ENOTCONN),
            None => Err(SystemError::EBADF),
        }
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let unspecified_addr = match self.ip_version {
            IpVersion::Ipv4 => UNSPECIFIED_LOCAL_ENDPOINT_V4.addr,
            IpVersion::Ipv6 => UNSPECIFIED_LOCAL_ENDPOINT_V6.addr,
        };

        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                let IpListenEndpoint { addr, port } = bound.endpoint();

                // If bound to "any" address (0.0.0.0 or ::), but connected to a specific address,
                // return the actual local address that would be used for the connection
                let local_addr = if let Some(addr) = addr {
                    addr
                } else {
                    // Socket is bound to ANY - check if connected
                    if let Ok(remote) = bound.remote_endpoint() {
                        // Connected: return the local address for the interface that can reach the remote
                        // For loopback, return loopback address; otherwise get from interface
                        match remote.addr {
                            Ipv4(addr) if addr.is_loopback() => Ipv4(addr),
                            Ipv6(addr) if addr.is_loopback() => Ipv6(addr),
                            _ => {
                                // Get the first IP address from the interface
                                let iface_guard = bound.inner().iface().smol_iface().lock();
                                if let Some(cidr) = iface_guard.ip_addrs().first() {
                                    cidr.address()
                                } else {
                                    unspecified_addr
                                }
                            }
                        }
                    } else {
                        // Not connected, return "any"
                        unspecified_addr
                    }
                };

                Ok(Endpoint::Ip(IpEndpoint::new(local_addr, port)))
            }
            Some(_) => match self.ip_version {
                IpVersion::Ipv4 => Ok(Endpoint::Ip(UNSPECIFIED_LOCAL_ENDPOINT_V4)),
                IpVersion::Ipv6 => Ok(Endpoint::Ip(UNSPECIFIED_LOCAL_ENDPOINT_V6)),
            },
            None => Err(SystemError::EBADF),
        }
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        // log::debug!(
        //     "recv_msg: msg_name={:?}, msg_namelen={}, flags={:?}",
        //     msg.msg_name,
        //     msg.msg_namelen,
        //     flags
        // );

        // Handle MSG_ERRQUEUE for socket error queue
        if flags.contains(PMSG::ERRQUEUE) {
            let entry = self
                .pop_errqueue()
                .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

            // Write offender address if requested
            let offender_ep = Endpoint::Ip(entry.offender);
            msg.msg_namelen = offender_ep.write_to_user_msghdr(msg.msg_name, msg.msg_namelen)?;

            // Prepare control message: sock_extended_err + offender sockaddr
            let err_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&entry.err as *const SockExtendedErr) as *const u8,
                    core::mem::size_of::<SockExtendedErr>(),
                )
            };
            let sockaddr = SockAddr::from(offender_ep);
            let sockaddr_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&sockaddr as *const SockAddr) as *const u8,
                    entry.addr_len,
                )
            };

            let mut data = alloc::vec::Vec::with_capacity(err_bytes.len() + sockaddr_bytes.len());
            data.extend_from_slice(err_bytes);
            data.extend_from_slice(sockaddr_bytes);

            msg.msg_flags = PMSG::ERRQUEUE.bits() as i32;
            let mut write_off = 0usize;
            let mut cmsg_buf = CmsgBuffer {
                ptr: msg.msg_control,
                len: msg.msg_controllen,
                write_off: &mut write_off,
            };
            cmsg_buf.put(
                &mut msg.msg_flags,
                entry.cmsg_level,
                entry.cmsg_type,
                data.len(),
                &data,
            )?;
            msg.msg_controllen = write_off;

            return Ok(0);
        }

        // Validate and create iovecs
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let mut buf = iovs.new_buf(true);
        let buf_cap = buf.len();

        // Receive data from socket
        let (copy_len, src_endpoint, orig_len, dst_addr, ifindex) = {
            let peek = flags.contains(PMSG::PEEK);
            if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
                let (copy_len, endpoint, orig_len, dst_addr, ifindex) =
                    self.try_recv_with_meta(&mut buf, peek)?;
                (
                    copy_len,
                    Endpoint::Ip(endpoint),
                    orig_len,
                    dst_addr,
                    ifindex,
                )
            } else {
                loop {
                    // Re-check shutdown state inside the loop
                    let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                    let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                    match self.try_recv_with_meta(&mut buf, peek) {
                        Ok((copy_len, endpoint, orig_len, dst_addr, ifindex)) => {
                            break (
                                copy_len,
                                Endpoint::Ip(endpoint),
                                orig_len,
                                dst_addr,
                                ifindex,
                            );
                        }
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                            // If shutdown and no data available, return EOF
                            if is_recv_shutdown {
                                if let Some(UdpInner::Bound(bound)) = self.inner.read().as_ref() {
                                    if let Ok(remote) = bound.remote_endpoint() {
                                        break (
                                            0,
                                            Endpoint::Ip(remote),
                                            0,
                                            self.unspecified_addr(),
                                            0,
                                        );
                                    }
                                }
                                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                            }
                            self.wait_queue.wait_event_io_interruptible_timeout(
                                || self.can_recv(),
                                self.recv_timeout(),
                            )?;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        };

        // log::debug!(
        //     "recv_msg: received {} bytes from {:?}",
        //     recv_size,
        //     src_endpoint
        // );

        // Scatter received data to user iovecs
        iovs.scatter(&buf[..copy_len])?;

        // Write source address if requested
        if !msg.msg_name.is_null() {
            let src_addr = msg.msg_name;
            // log::debug!(
            //     "recv_msg: writing endpoint to user, msg_namelen={}",
            //     msg.msg_namelen
            // );
            let actual_len = src_endpoint.write_to_user_msghdr(src_addr, msg.msg_namelen)?;
            msg.msg_namelen = actual_len;
            // log::debug!(
            //     "recv_msg: endpoint written, updated msg_namelen={}",
            //     msg.msg_namelen
            // );
        } else {
            // log::debug!("recv_msg: msg_name is NULL, skipping endpoint write");
            msg.msg_namelen = 0;
        }

        let cmsg_len = msg.msg_controllen;
        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        if orig_len > buf_cap {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }
        if cmsg_len > 0 {
            let mut write_off = 0usize;
            let mut cmsg_buf = CmsgBuffer {
                ptr: msg.msg_control,
                len: cmsg_len,
                write_off: &mut write_off,
            };
            self.build_udp_recv_cmsgs(&mut cmsg_buf, &mut msg.msg_flags, dst_addr, ifindex)?;
            msg.msg_controllen = write_off;
        }

        // log::debug!("recv_msg: returning {} bytes", recv_size);
        Ok(Self::recv_return_len(copy_len, orig_len, flags))
    }

    fn send_msg(&self, msg: &crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        // Validate and gather iovecs
        // TODO: Actual iovecs sends
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let data = iovs.gather()?;

        // Check if destination address is provided
        if !msg.msg_name.is_null() && msg.msg_namelen > 0 {
            // Send to specific address
            let endpoint = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            self.send_to(&data, flags, endpoint)
        } else {
            // Send using connected endpoint
            self.send(&data, flags)
        }
    }

    fn epoll_items(&self) -> &crate::net::socket::common::EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn check_io_event(&self) -> EPollEventType {
        let mut event = EPollEventType::empty();
        let loopback_has_data = !self.multicast_loopback_rx.lock().is_empty();
        match self.inner.read().as_ref() {
            Some(UdpInner::Unbound(_)) => {
                event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
            }
            Some(UdpInner::Bound(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv || loopback_has_data {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                }
            }
            None => {
                // Socket is closed
                event.insert(EP::EPOLLERR | EP::EPOLLHUP);
            }
        }
        event
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }

    fn send_bytes_available(&self) -> Result<usize, SystemError> {
        Ok(match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity() - socket.send_queue())
            }
            _ => 0,
        })
    }
}

impl InetSocket for UdpSocket {
    fn on_iface_events(&self) {
        // Wake up any threads waiting on this socket
        self.wait_queue.wakeup_all(None);

        // Notify epoll/poll watchers about socket state changes
        let pollflag = self.check_io_event();
        let _ = EventPoll::wakeup_epoll(self.epoll_items().as_ref(), pollflag);
    }
}
