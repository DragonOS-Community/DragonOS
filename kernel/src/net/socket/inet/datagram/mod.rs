use inner::{BoundUdp, UdpBindContext, UdpInner, UnboundUdp};
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
use crate::process::namespace::NamespaceOps;
use crate::process::ProcessManager;
use crate::time::{Duration, Instant};
use crate::{libs::rwsem::RwSem, net::socket::endpoint::Endpoint};
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use smoltcp::wire::{IpAddress::*, IpEndpoint, IpListenEndpoint, IpVersion, Ipv4Address};

use super::{
    common::{
        bind_address_uses_local_owner, ensure_bound_dual_stack_remote_compatible,
        DeviceBindingUpdate, EphemeralBindTarget, SocketDeviceBinding,
    },
    InetSocket, UNSPECIFIED_LOCAL_ENDPOINT_V4, UNSPECIFIED_LOCAL_ENDPOINT_V6,
};

mod option;
mod output_flow;
mod socket_impl;

pub mod inner;
pub mod multicast_loopback;
pub(crate) mod udp_bindings;

type EP = crate::filesystem::epoll::EPollEventType;
const IFACE_POLL_BATCH_ROUNDS: usize = 128;

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
    /// Stabilizes the smoltcp socket's interface placement across send-side
    /// polling. Ordinary unicast sends share the read side; temporary interface
    /// moves and control/lifecycle updates take the write side.
    iface_placement: RwSem<()>,
    nonblock: AtomicBool,
    shutdown: AtomicU8,
    wait_queue: WaitQueue,
    inode_id: InodeId,
    open_files: AtomicUsize,
    fsnotify_watches: AtomicUsize,
    self_ref: Weak<UdpSocket>,
    netns: Arc<NetNamespace>,
    /// SO_BINDTODEVICE authoritative interface index.
    device_binding: SocketDeviceBinding,
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
    /// IP_MULTICAST_ALL
    ip_multicast_all: AtomicBool,
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
            iface_placement: RwSem::new(()),
            nonblock: AtomicBool::new(nonblock),
            shutdown: AtomicU8::new(0),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            fsnotify_watches: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            device_binding: SocketDeviceBinding::default(),
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
            ip_multicast_all: AtomicBool::new(true),
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

    fn bind_context(&self) -> UdpBindContext {
        UdpBindContext {
            netns: self.netns(),
            socket: self.self_ref.clone(),
            reuseaddr: self.so_reuseaddr.load(Ordering::Relaxed),
            reuseport: self.so_reuseport.load(Ordering::Relaxed),
            bind_id: self.bind_id(),
            bound_ifindex: self.bound_device_ifindex(),
        }
    }

    #[inline]
    pub(crate) fn bound_device_ifindex(&self) -> usize {
        self.device_binding.ifindex()
    }

    #[inline]
    pub(crate) fn device_binding_allows(&self, ifindex: usize) -> bool {
        self.device_binding.allows(ifindex)
    }

    #[inline]
    pub(crate) fn reuse_options(&self) -> (bool, bool) {
        (
            self.so_reuseaddr.load(Ordering::Relaxed),
            self.so_reuseport.load(Ordering::Relaxed),
        )
    }

    fn apply_device_binding(
        &self,
        update: &mut DeviceBindingUpdate<'_>,
    ) -> Result<(), SystemError> {
        // `prepare_update()` already holds the device-binding writer lock.
        // Keep this ordering consistent with send: binding writer -> placement
        // writer -> inner. This prevents a multicast send from restoring an
        // interface after a newer SO_BINDTODEVICE update has committed.
        let _placement = self.iface_placement.write();
        let mut inner = self.inner.write();
        match inner.as_mut().ok_or(SystemError::EBADF)? {
            UdpInner::Unbound(_) => update.commit(),
            UdpInner::Bound(bound) => {
                let target_iface = update.target_iface();
                let old_iface = bound.inner().iface().clone();
                // A concrete endpoint must stay in the stack selected by its
                // local-delivery route. SO_BINDTODEVICE constrains packet I/O;
                // it does not transfer transport ownership to the device.
                let placement_iface = match bound.endpoint().addr {
                    Some(addr) if bind_address_uses_local_owner(addr) => {
                        crate::net::socket::inet::common::get_iface_for_local_bind(
                            &addr,
                            &self.netns,
                        )
                    }
                    Some(addr) if !addr.is_unspecified() => target_iface.or_else(|| {
                        crate::net::socket::inet::common::get_iface_for_local_bind(
                            &addr,
                            &self.netns,
                        )
                    }),
                    _ => target_iface.or_else(|| {
                        crate::net::socket::inet::common::select_iface_for_unspecified(
                            &self.unspecified_addr(),
                            &self.netns,
                        )
                        .ok()
                    }),
                };
                let Some(placement_iface) = placement_iface else {
                    // Clearing sk_bound_dev_if must not depend on route
                    // selection or on an interface being available.
                    update.commit();
                    return Ok(());
                };
                if old_iface.nic_id() == placement_iface.nic_id() {
                    update.commit();
                    return Ok(());
                }

                bound
                    .inner_mut()
                    .move_udp_to_iface_with(placement_iface.clone(), || update.commit())?;
                if let Some(socket) = self.self_ref.upgrade() {
                    old_iface.common().unbind_socket(socket.clone());
                    placement_iface.common().bind_socket(socket);
                }
            }
        }
        Ok(())
    }

    #[inline]
    fn unspecified_addr(&self) -> smoltcp::wire::IpAddress {
        match self.ip_version {
            IpVersion::Ipv4 => smoltcp::wire::IpAddress::v4(0, 0, 0, 0),
            IpVersion::Ipv6 => smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0),
        }
    }

    /// Places an explicitly addressed UDP endpoint according to its transport
    /// owner. Device binding is an independent I/O constraint and selects the
    /// SocketSet only for wildcard, multicast, or broadcast endpoints.
    fn bind_endpoint_on_owner(
        &self,
        unbound: UnboundUdp,
        endpoint: smoltcp::wire::IpEndpoint,
        device_iface: Option<Arc<dyn Iface>>,
    ) -> Result<BoundUdp, SystemError> {
        if bind_address_uses_local_owner(endpoint.addr) {
            unbound.bind(endpoint, self.bind_context())
        } else if let Some(iface) = device_iface {
            unbound.bind_on_iface(iface, endpoint, self.bind_context())
        } else {
            unbound.bind(endpoint, self.bind_context())
        }
    }

    #[inline]
    fn normalize_unspecified_dest(dest: IpEndpoint) -> IpEndpoint {
        if !dest.addr.is_unspecified() {
            return dest;
        }

        let addr = match dest.addr {
            Ipv4(_) => smoltcp::wire::IpAddress::v4(127, 0, 0, 1),
            Ipv6(_) => smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1),
        };
        IpEndpoint::new(addr, dest.port)
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    #[inline]
    fn poll_iface_until_quiescent(iface: &dyn crate::net::Iface) {
        loop {
            let mut progressed = false;
            for i in 0..IFACE_POLL_BATCH_ROUNDS {
                if !iface.poll() {
                    return;
                }
                progressed = true;
                if (i & 0x7) == 0x7 {
                    let pcb = ProcessManager::current_pcb();
                    if pcb.has_pending_signal_fast() && pcb.has_pending_not_masked_signal() {
                        return;
                    }
                }
            }
            if progressed {
                crate::sched::sched_yield();
            } else {
                return;
            }
        }
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

    fn send_timeout(&self) -> Option<crate::time::Duration> {
        let us = self
            .send_timeout_us
            .load(core::sync::atomic::Ordering::Relaxed);
        if us == u64::MAX {
            None
        } else {
            Some(crate::time::Duration::from_micros(us))
        }
    }

    #[inline]
    fn loopback_send_len_result(
        payload_len: usize,
        max_payload: usize,
    ) -> Result<usize, SystemError> {
        if payload_len > max_payload || payload_len > u16::MAX as usize {
            Err(SystemError::EMSGSIZE)
        } else {
            Ok(payload_len)
        }
    }

    #[inline]
    fn validate_bound_send_dest(
        &self,
        bound: &inner::BoundUdp,
        dest: IpEndpoint,
    ) -> Result<(), SystemError> {
        if self.ip_version != IpVersion::Ipv6 {
            return Ok(());
        }

        if let Some(local_addr) = bound.endpoint().addr {
            ensure_bound_dual_stack_remote_compatible(local_addr, dest.addr)?;
        }

        Ok(())
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
        let _placement = self.iface_placement.write();
        let mut inner = self.inner.write();

        // Check socket state first without taking
        match inner.as_ref() {
            None => return Err(SystemError::EBADF),
            Some(UdpInner::Bound(_)) => return Err(SystemError::EINVAL), // Already bound
            Some(UdpInner::Unbound(_)) => {}
        }
        let bound_iface = self.device_binding.resolve_iface(&self.netns)?;
        if bound_iface.is_some()
            && !local_endpoint.addr.is_unspecified()
            && crate::net::socket::inet::common::get_iface_for_local_bind(
                &local_endpoint.addr,
                &self.netns,
            )
            .is_none()
        {
            return Err(SystemError::EADDRNOTAVAIL);
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

        let result = self.bind_endpoint_on_owner(unbound, local_endpoint, bound_iface);
        match result {
            Ok(bound) => {
                bound
                    .inner()
                    .iface()
                    .common()
                    .bind_socket(self.self_ref.upgrade().unwrap());
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
        let device_target = self.device_ephemeral_bind_target(remote)?;
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

                let bound_result = if let Some(target) = device_target {
                    inner.bind_ephemeral_on_iface(
                        target.stack_owner,
                        target.local_addr,
                        self.bind_context(),
                    )
                } else if let Some((iface, local_addr)) =
                    self.ipv4_multicast_ephemeral_bind_target(remote)
                {
                    inner.bind_ephemeral_on_iface(iface, local_addr, self.bind_context())
                } else {
                    inner.bind_ephemeral(remote, self.bind_context())
                };
                match bound_result {
                    Ok(bound) => {
                        newly_bound_iface = Some(bound.inner().iface().clone());
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
        let _placement = self.iface_placement.write();
        let mut inner_guard = self.inner.write();

        // Check if socket is bound
        let bound = match inner_guard.as_ref() {
            Some(UdpInner::Bound(b)) => b,
            _ => return Ok(()), // Not bound, nothing to do
        };

        // Save current state before recreating
        let local_ep = bound.endpoint();
        let remote_ep = bound.remote_endpoint().ok(); // May be None if not connected
        let connected_source = bound.connected_source();
        let explicitly_bound = !bound.should_unbind_on_disconnect();
        let old_iface = bound.inner().iface().clone();
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
            old_iface
                .common()
                .unbind_socket(self.self_ref.upgrade().unwrap());
            self.netns.udp_bindings().unbind(port, self.bind_id());
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
        // Resizing is an implementation detail, not a new bind operation.
        // Preserve the already validated SocketSet owner even if address/FIB
        // state or SO_BINDTODEVICE changed after the original bind.
        let bound = match unbound.bind_on_iface(old_iface, new_endpoint, self.bind_context()) {
            Ok(b) => b,
            Err(e) => {
                // Restore unbound state on error
                *inner_guard = Some(UdpInner::Unbound(UnboundUdp::new()));
                return Err(e);
            }
        };

        // Restore connection if it existed
        let mut bound = bound;
        bound.set_explicitly_bound(explicitly_bound);
        if let Some(remote) = remote_ep {
            bound.connect(remote, connected_source);
        }

        // Restore the binding in the interface
        bound
            .inner()
            .iface()
            .common()
            .bind_socket(self.self_ref.upgrade().unwrap());
        *inner_guard = Some(UdpInner::Bound(bound));

        Ok(())
    }

    pub fn close(&self) {
        let _placement = self.iface_placement.write();
        let mut inner = self.inner.write();
        if let Some(UdpInner::Bound(bound)) = &mut *inner {
            self.netns
                .udp_bindings()
                .unbind(bound.endpoint().port, self.bind_id());
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

    fn disconnect_udp(&self) -> Result<(), SystemError> {
        let _placement = self.iface_placement.write();
        let mut inner_guard = self.inner.write();
        let should_unbind = match inner_guard.as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.disconnect();
                bound.should_unbind_on_disconnect()
            }
            Some(UdpInner::Unbound(_)) => return Ok(()),
            None => return Err(SystemError::EBADF),
        };

        if should_unbind {
            let Some(UdpInner::Bound(bound)) = inner_guard.take() else {
                unreachable!();
            };
            self.netns
                .udp_bindings()
                .unbind(bound.endpoint().port, self.bind_id());
            multicast_loopback::multicast_registry().unregister_all(&self.self_ref);
            bound
                .inner()
                .iface()
                .common()
                .unbind_socket(self.self_ref.upgrade().unwrap());
            bound.close();
            inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
        }
        Ok(())
    }

    pub fn try_recv(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<(usize, smoltcp::wire::IpEndpoint, usize), SystemError> {
        let (copy_len, endpoint, orig_len, _, _) = self.try_recv_with_meta(buf, peek)?;
        Ok((copy_len, endpoint, orig_len))
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
        let bound_ifindex = bound.inner().iface().nic_id() as i32;
        let device_ifindex = self.bound_device_ifindex();
        let (copy_len, endpoint, orig_len, dst_addr, meta) =
            bound.try_recv_with_metadata(buf, peek, device_ifindex, bound_ifindex as usize)?;
        let ifindex = i32::try_from(meta.id)
            .ok()
            .filter(|ifindex| *ifindex != 0)
            .unwrap_or(bound_ifindex);
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

    fn ipv4_multicast_ephemeral_bind_target(
        &self,
        dest: smoltcp::wire::IpAddress,
    ) -> Option<(Arc<dyn Iface>, smoltcp::wire::IpAddress)> {
        if !matches!(dest, Ipv4(addr) if addr.is_multicast()) {
            return None;
        }

        let ifindex = self.ip_multicast_ifindex.load(Ordering::Relaxed);
        let ifaddr = self.ip_multicast_addr.load(Ordering::Relaxed);
        if ifindex == 0 && ifaddr == 0 {
            return None;
        }

        let iface = if ifindex != 0 {
            crate::net::socket::inet::common::multicast::find_iface_by_ifindex(&self.netns, ifindex)
        } else {
            crate::net::socket::inet::common::multicast::find_iface_by_ipv4(&self.netns, ifaddr)
        }?;

        if ifaddr != 0 {
            let octets = ifaddr.to_ne_bytes();
            return Some((
                iface,
                Ipv4(Ipv4Address::new(octets[0], octets[1], octets[2], octets[3])),
            ));
        }

        let local_addr = {
            let smol_iface = iface.smol_iface().lock();
            smol_iface
                .ip_addrs()
                .iter()
                .find_map(|cidr| match cidr.address() {
                    Ipv4(addr) => Some(Ipv4(addr)),
                    _ => None,
                })
        }?;

        Some((iface, local_addr))
    }

    fn device_ephemeral_bind_target(
        &self,
        remote: smoltcp::wire::IpAddress,
    ) -> Result<Option<EphemeralBindTarget>, SystemError> {
        let Some(iface) = self.device_binding.resolve_iface(&self.netns)? else {
            return Ok(None);
        };
        let target = crate::net::socket::inet::common::ephemeral_bind_target_on_iface(
            &self.netns,
            iface,
            &remote,
        )?;
        Ok(Some(target))
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

    fn enqueue_ipv6_emsgsize_errqueue(&self, payload_len: usize, offender: Option<IpEndpoint>) {
        if self.ip_version != IpVersion::Ipv6 || !self.recv_err_v6.load(Ordering::Acquire) {
            return;
        }

        let mut off = offender.unwrap_or_else(|| IpEndpoint::new(self.unspecified_addr(), 0));
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
            ee_info: payload_len as u32,
            ee_data: 0,
        };
        let addr_len = SockAddr::from(Endpoint::Ip(off)).len().unwrap_or(0) as usize;
        if addr_len != 0 {
            self.enqueue_errqueue(err, off, PSOL::IPV6 as i32, PIPV6::RECVERR as i32, addr_len);
        }
    }

    fn connected_or_explicit_send_dest(&self, address: Option<&Endpoint>) -> Option<IpEndpoint> {
        if let Some(Endpoint::Ip(dest)) = address {
            return Some(Self::normalize_unspecified_dest(*dest));
        }

        self.inner
            .read()
            .as_ref()
            .and_then(|inner| match inner {
                UdpInner::Bound(bound) => bound.remote_endpoint().ok(),
                _ => None,
            })
            .map(Self::normalize_unspecified_dest)
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

        let placement = self.iface_placement.read();
        let explicit = to.map(Endpoint::Ip);
        let is_multicast = self
            .connected_or_explicit_send_dest(explicit.as_ref())
            .is_some_and(|dest| dest.addr.is_multicast());
        if is_multicast {
            drop(placement);
            let _placement = self.iface_placement.write();
            return self.try_send_with_stable_iface(buf, to);
        }

        self.try_send_with_stable_iface(buf, to)
    }

    fn try_send_with_stable_iface(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        // The caller's placement guard intentionally spans polling below.
        // `inner` cannot remain locked because notifications may re-enter
        // socket event checks.

        // Resolve wildcard unicast flow state before taking the send-side
        // inner lock. The placement guard keeps endpoint/device state stable
        // across this lookup and the later enqueue.
        let output_flow_request = {
            let inner = self.inner.read();
            if let Some(UdpInner::Bound(bound)) = inner.as_ref() {
                to.or_else(|| bound.remote_endpoint().ok())
                    .map(Self::normalize_unspecified_dest)
                    .map(|dest| (bound.endpoint(), dest, bound.connected_source()))
            } else {
                None
            }
        };
        let mut resolved_output_flow = match output_flow_request {
            Some((local, dest, connected_source)) => output_flow::resolve_wildcard_ipv4(
                &self.netns,
                local,
                dest.addr,
                (self.bound_device_ifindex() != 0).then_some(self.bound_device_ifindex() as u32),
                connected_source,
                dest.addr.is_multicast(),
                dest.addr.is_broadcast(),
            )?,
            None => None,
        };

        // Send data and snapshot the delivery metadata before releasing inner.
        let (
            result,
            send_iface,
            dest,
            dest_is_broadcast,
            loopback_send,
            send_iface_is_loopback,
            local_delivery_ifindex,
            src_endpoint,
            restore_iface,
        ) = {
            let mut inner_guard = self.inner.write();

            // Check if socket is closed
            let inner = inner_guard.as_ref().ok_or(SystemError::EBADF)?;

            // If unbound, bind to ephemeral port
            if let UdpInner::Unbound(_) = inner {
                let to_addr =
                    Self::normalize_unspecified_dest(to.ok_or(SystemError::EDESTADDRREQ)?).addr;
                let device_target = self.device_ephemeral_bind_target(to_addr)?;
                let unbound = match inner_guard.take().unwrap() {
                    UdpInner::Unbound(unbound) => unbound,
                    _ => unreachable!(),
                };
                let bound_result = if let Some(target) = device_target {
                    unbound.bind_ephemeral_on_iface(
                        target.stack_owner,
                        target.local_addr,
                        self.bind_context(),
                    )
                } else if let Some((iface, local_addr)) =
                    self.ipv4_multicast_ephemeral_bind_target(to_addr)
                {
                    unbound.bind_ephemeral_on_iface(iface, local_addr, self.bind_context())
                } else {
                    unbound.bind_ephemeral(to_addr, self.bind_context())
                };
                match bound_result {
                    Ok(bound) => {
                        // Register for iface notifications on implicit bind via sendto().
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
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
                    self.device_binding.resolve_iface(&self.netns)?;
                    let dest = to
                        .or_else(|| bound.remote_endpoint().ok())
                        .ok_or(SystemError::EDESTADDRREQ)?;
                    let dest = Self::normalize_unspecified_dest(dest);
                    self.validate_bound_send_dest(bound, dest)?;
                    let bound_iface = bound.inner().iface().clone();
                    let is_multicast = dest.addr.is_multicast();
                    let device_ifindex = self.bound_device_ifindex() as i32;
                    let mcast_ifindex = if is_multicast && device_ifindex != 0 {
                        device_ifindex
                    } else if is_multicast {
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
                    let loopback_broadcast = self
                        .netns
                        .loopback_iface()
                        .map(|lo| lo.smol_iface().lock().inner.is_broadcast(&dest.addr))
                        .unwrap_or(false);
                    let bound_iface_is_broadcast = bound_iface
                        .smol_iface()
                        .lock()
                        .inner
                        .is_broadcast(&dest.addr);
                    let is_broadcast = loopback_broadcast || bound_iface_is_broadcast;
                    let output_flow = if is_multicast || is_broadcast {
                        None
                    } else {
                        resolved_output_flow.take()
                    };
                    let local_delivery_ifindex =
                        if device_ifindex != 0 && (is_multicast || bound_iface_is_broadcast) {
                            Some(device_ifindex)
                        } else if is_multicast && mcast_ifindex != 0 {
                            Some(mcast_ifindex)
                        } else {
                            crate::net::socket::inet::common::get_iface_to_bind(
                                &dest.addr,
                                self.netns(),
                            )
                            .map(|iface| iface.nic_id() as i32)
                        };
                    let binding_allows_local_delivery = local_delivery_ifindex
                        .is_some_and(|ifindex| device_ifindex == 0 || device_ifindex == ifindex);
                    let should_loopback_send = binding_allows_local_delivery
                        && ((!is_multicast && local_delivery_ifindex.is_some())
                            || (is_multicast && (send_iface_is_loopback || loopback_broadcast)));
                    let src_endpoint = output_flow::local_source_endpoint(
                        &self.netns,
                        bound.endpoint(),
                        dest.addr,
                        local_delivery_ifindex,
                        output_flow,
                        self.unspecified_addr(),
                    );
                    if should_loopback_send {
                        let max_payload =
                            bound.with_socket(|socket| socket.payload_send_capacity());
                        (
                            Self::loopback_send_len_result(buf.len(), max_payload),
                            bound_iface,
                            Some(dest),
                            is_broadcast,
                            true,
                            send_iface_is_loopback,
                            local_delivery_ifindex,
                            src_endpoint,
                            None,
                        )
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

                        let egress_ifindex = if let Some(flow) = output_flow {
                            flow.oif
                        } else if device_ifindex != 0 {
                            device_ifindex as u32
                        } else if is_multicast && mcast_ifindex != 0 {
                            mcast_ifindex as u32
                        } else {
                            0
                        };
                        let ret = bound.try_send(
                            buf,
                            Some(dest),
                            egress_ifindex,
                            output_flow.map(|flow| flow.source),
                        );
                        (
                            ret,
                            send_iface,
                            Some(dest),
                            is_broadcast,
                            false,
                            send_iface_is_loopback,
                            local_delivery_ifindex,
                            src_endpoint,
                            restore_iface,
                        )
                    }
                }
                _ => return Err(SystemError::ENOTCONN),
            }
        }; // `inner` is released here.

        if loopback_send {
            if let Some(dest) = dest {
                let ifindex = local_delivery_ifindex.unwrap_or_else(|| send_iface.nic_id() as i32);
                if dest.addr.is_multicast() {
                    if let Ipv4(addr) = dest.addr {
                        let octets = addr.octets();
                        let multiaddr = u32::from_ne_bytes(octets);
                        if multicast_loopback::multicast_registry().has_membership(
                            self.netns.ns_common().nsid.data(),
                            multiaddr,
                            ifindex,
                        ) {
                            self.netns.udp_bindings().deliver_multicast(
                                dest,
                                src_endpoint,
                                ifindex,
                                buf,
                            );
                        }
                    }
                } else if dest_is_broadcast {
                    self.netns
                        .udp_bindings()
                        .deliver_broadcast(dest, src_endpoint, ifindex, buf);
                } else {
                    self.netns
                        .udp_bindings()
                        .deliver_unicast(dest, src_endpoint, ifindex, buf);
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
                self.enqueue_ipv6_emsgsize_errqueue(buf.len(), dest);
            }
            return result;
        }

        // Poll after releasing inner. This is required because polling notifies all
        // sockets on the interface and may re-enter this socket's event checks.
        Self::poll_iface_until_quiescent(send_iface.as_ref());

        if let Some(orig_iface) = restore_iface {
            let mut inner_guard = self.inner.write();
            if let Some(UdpInner::Bound(bound)) = inner_guard.as_mut() {
                let _ = bound.inner_mut().move_udp_to_iface(orig_iface);
            }
        }

        // Multicast loopback: if sending to a multicast address and loopback is enabled,
        // deliver the packet to all local sockets that have joined the group
        if result.is_ok() {
            if let Some(dest) = dest {
                let allow_mcast_loop =
                    self.is_multicast_loopback_enabled() || send_iface_is_loopback;
                if dest.addr.is_multicast() && allow_mcast_loop {
                    // Get multicast address and interface index
                    if let Ipv4(addr) = dest.addr {
                        let octets = addr.octets();
                        let multiaddr = u32::from_ne_bytes(octets);
                        let ifindex =
                            local_delivery_ifindex.unwrap_or_else(|| self.get_multicast_ifindex());

                        if multicast_loopback::multicast_registry().has_membership(
                            self.netns.ns_common().nsid.data(),
                            multiaddr,
                            ifindex,
                        ) {
                            self.netns.udp_bindings().deliver_multicast(
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
            self.enqueue_ipv6_emsgsize_errqueue(buf.len(), to);
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
        if ifindex <= 0 || !self.device_binding.allows(ifindex as usize) {
            return false;
        }
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
