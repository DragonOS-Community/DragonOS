use inner::{BoundUdp, UdpBindContext, UdpInner, UnboundUdp};
use smoltcp;
use smoltcp::wire::IpAddress;
use system_error::SystemError;

use super::common::error_queue::{
    ErrorQueue, ErrorQueueEntry, SockExtendedErr, SO_EE_ORIGIN_ICMP, SO_EE_ORIGIN_ICMP6,
    SO_EE_ORIGIN_LOCAL,
};
use super::common::pmtu::PmtuPolicy;
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
use crate::time::{Duration, Instant};
use crate::{libs::rwsem::RwSem, net::socket::endpoint::Endpoint};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use smoltcp::wire::{IpAddress::*, IpEndpoint, IpListenEndpoint, IpVersion};

use super::{
    common::{
        bind_address_uses_local_owner, ensure_bound_dual_stack_remote_compatible,
        DeviceBindingUpdate, EphemeralBindTarget, SocketDeviceBinding,
    },
    InetSocket, UNSPECIFIED_LOCAL_ENDPOINT_V4, UNSPECIFIED_LOCAL_ENDPOINT_V6,
};

mod ipv4_packet;
mod ipv6_packet;
mod option;
mod output_flow;
mod rx_queue;
mod socket_impl;

pub mod inner;
mod ipv6_multicast;
pub mod multicast_loopback;
pub(crate) mod udp_bindings;

type EP = crate::filesystem::epoll::EPollEventType;

struct Ipv4SendSnapshot {
    owner: Arc<dyn Iface>,
    source: smoltcp::wire::Ipv4Address,
    destination: smoltcp::wire::Ipv4Address,
    source_port: u16,
    destination_port: u16,
    route: crate::net::route::OutputRouteDecision,
    ttl: u8,
    multicast_loop: bool,
    checksum_enabled: bool,
}

struct Ipv6SendSnapshot {
    owner: Arc<dyn Iface>,
    route: crate::net::route::OutputRouteDecision,
    source: smoltcp::wire::Ipv6Address,
    destination: smoltcp::wire::Ipv6Address,
    source_port: u16,
    destination_port: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct InPktInfo {
    ipi_ifindex: i32,
    ipi_spec_dst: u32,
    ipi_addr: u32,
}

// Udp Socket 负责提供状态切换接口、执行状态切换
#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct UdpSocket {
    data: crate::libs::lazy_init::Lazy<crate::cgroup::memory::owned::OwnedBox<UdpSocketData>>,
}

impl core::ops::Deref for UdpSocket {
    type Target = UdpSocketData;
    fn deref(&self) -> &Self::Target {
        self.data.get()
    }
}

#[derive(Debug)]
pub struct UdpSocketData {
    memory: crate::net::socket::buffer::SocketMemory,
    /// Preparation may allocate, but never while holding inner/SocketSet.
    resize_lock: Mutex<()>,
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
    /// Pending IPv4 UDP bytes remain charged across SocketSet replacement.
    send_account: Arc<super::common::output_account::SocketOutputAccount<UdpSocket>>,
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
    errqueue: Mutex<(i32, ErrorQueue)>,
    pmtu_v4: AtomicI32,
    pmtu_v6: AtomicI32,
    ipv6_mtu: AtomicUsize,
    ipv6_multicast_loop: AtomicBool,
    ipv6_multicast_hops: AtomicI32,
    ipv6_multicast_ifindex: AtomicU32,
    ipv6_multicast_all: AtomicBool,
    ipv6_multicast_groups: Mutex<Vec<ipv6_multicast::Membership>>,
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
    /// One Linux-compatible receive queue shared by physical and local ingress.
    receive_queue: rx_queue::UdpReceiveQueue,
}

impl UdpSocket {
    pub fn new(nonblock: bool, version: IpVersion) -> Result<Arc<Self>, SystemError> {
        let netns = ProcessManager::current_netns();
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        loop {
            let task = ProcessManager::current_pcb();
            let account = crate::cgroup::memory::account_for_task(&task);
            let creator = crate::cgroup::memory::object_for_task(&task);
            let prepared = (|| {
                let memory = crate::net::socket::buffer::SocketMemory::prepare(&account, creator)?;
                let body = crate::cgroup::memory::owned::OwnedBox::<UdpSocketData>::prepare(
                    &memory.creator,
                    crate::cgroup::memory::accounting::KernelByteKind::SlabUnreclaimable,
                )?
                .try_allocate()
                .map_err(|_| SystemError::ENOMEM)?;
                let unbound = UnboundUdp::try_new_with_buf_size(version, 0, 0, &memory)?;
                let socket = Arc::try_new(Self {
                    data: crate::libs::lazy_init::Lazy::new(),
                })
                .map_err(|_| SystemError::ENOMEM)?;
                let me = Arc::downgrade(&socket);
                let send_account =
                    Arc::try_new(super::common::output_account::SocketOutputAccount::new(
                        me.clone(),
                        inner::DEFAULT_TX_BUF_SIZE * 2,
                    ))
                    .map_err(|_| SystemError::ENOMEM)?;
                Ok::<_, crate::mm::allocation::MemoryAllocationError>((
                    memory,
                    body,
                    unbound,
                    socket,
                    me,
                    send_account,
                ))
            })();
            let (memory, body, unbound, socket, me, send_account) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    error.recover(&mut recovery)?;
                    continue;
                }
            };
            // Weak is only saved into private owners; no registration or
            // upgrade may observe the thin identity before body initialization.
            socket.data.init(body.init(UdpSocketData {
                memory,
                resize_lock: Mutex::new(()),
                inner: RwSem::new(Some(UdpInner::Unbound(unbound))),
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
                send_account,
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
                errqueue: Mutex::new((0, ErrorQueue::default())),
                pmtu_v4: AtomicI32::new(PmtuPolicy::Want.as_i32()),
                pmtu_v6: AtomicI32::new(PmtuPolicy::Want.as_i32()),
                ipv6_mtu: AtomicUsize::new(0),
                ipv6_multicast_loop: AtomicBool::new(true),
                ipv6_multicast_hops: AtomicI32::new(1),
                ipv6_multicast_ifindex: AtomicU32::new(0),
                ipv6_multicast_all: AtomicBool::new(true),
                ipv6_multicast_groups: Mutex::new(Vec::new()),
                linger_onoff: AtomicI32::new(0),
                linger_linger: AtomicI32::new(0),
                send_timeout_us: AtomicU64::new(u64::MAX),
                recv_timeout_us: AtomicU64::new(u64::MAX),
                no_check: AtomicBool::new(false), // checksums enabled by default
                ip_version: version,
                receive_queue: rx_queue::UdpReceiveQueue::new(inner::DEFAULT_RX_BUF_SIZE),
            }));
            return Ok(socket);
        }
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
            generation: self.receive_queue.begin_binding(),
            bound_ifindex: self.bound_device_ifindex(),
        }
    }

    #[inline]
    pub(crate) fn bound_device_ifindex(&self) -> usize {
        self.device_binding.ifindex()
    }

    #[inline]
    pub(crate) fn reuse_options(&self) -> (bool, bool) {
        (
            self.so_reuseaddr.load(Ordering::Relaxed),
            self.so_reuseport.load(Ordering::Relaxed),
        )
    }

    fn set_device_binding(&self, value: &[u8]) -> Result<(), SystemError> {
        loop {
            let target = {
                let update = self.device_binding.prepare_update(&self.netns, value)?;
                self.device_placement_target(&update)?
            };
            let mut prepared = match target.as_ref() {
                Some(target) => Some(self.prepare_iface_socket(target.clone())?),
                None => None,
            };
            let mut retired = None;
            let mut update = self.device_binding.prepare_update(&self.netns, value)?;
            let _placement = self.iface_placement.write();
            let current = self.device_placement_target(&update)?;
            if !match (&target, &current) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            } {
                continue;
            }
            let mut inner = self.inner.write();
            if let Some(UdpInner::Bound(bound)) = inner.as_mut() {
                if let Some(prepared) = prepared.as_mut() {
                    retired =
                        prepared.move_bound(bound.inner_mut(), self.self_ref.upgrade().unwrap())?;
                }
            } else if inner.is_none() {
                return Err(SystemError::EBADF);
            }
            update.commit();
            if let Some(UdpInner::Bound(bound)) = inner.as_ref() {
                bound.update_receive_filter(self.bound_device_ifindex());
            }
            return Ok(());
        }
    }

    fn device_placement_target(
        &self,
        update: &DeviceBindingUpdate<'_>,
    ) -> Result<Option<Arc<dyn Iface>>, SystemError> {
        let inner = self.inner.read();
        match inner.as_ref().ok_or(SystemError::EBADF)? {
            UdpInner::Unbound(_) => Ok(None),
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
                Ok(placement_iface.filter(|target| !Arc::ptr_eq(target, &old_iface)))
            }
        }
    }

    #[inline]
    fn unspecified_addr(&self) -> smoltcp::wire::IpAddress {
        match self.ip_version {
            IpVersion::Ipv4 => smoltcp::wire::IpAddress::v4(0, 0, 0, 0),
            IpVersion::Ipv6 => smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0),
        }
    }

    /// A netdevice move retires its old SocketSet. A live socket remains in
    /// its source namespace, so any later operation that needs a polled
    /// SocketSet chooses an interface still owned by that namespace.
    fn active_owner_for_bound(&self, bound: &BoundUdp) -> Result<Arc<dyn Iface>, SystemError> {
        let old = bound.inner().iface();
        if old
            .net_namespace()
            .is_some_and(|owner| Arc::ptr_eq(&owner, &self.netns))
        {
            return Ok(old.clone());
        }
        let local = bound.endpoint().addr;
        local
            .filter(|addr| !addr.is_unspecified())
            .and_then(|addr| {
                crate::net::socket::inet::common::get_iface_for_local_bind(&addr, &self.netns)
            })
            .or_else(|| self.netns.default_iface())
            .or_else(|| self.netns.loopback_iface().map(|lo| lo as Arc<dyn Iface>))
            .ok_or(SystemError::ENODEV)
    }

    fn prepare_iface_socket(
        &self,
        iface: Arc<dyn Iface>,
    ) -> Result<super::common::registration::PreparedIfaceSocket, SystemError> {
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        loop {
            let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
            match super::common::registration::PreparedIfaceSocket::try_new(
                iface.clone(),
                self.netns.clone(),
                &creator,
            ) {
                Ok(prepared) => return Ok(prepared),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }

    #[inline]
    fn normalize_unspecified_dest(dest: IpEndpoint) -> IpEndpoint {
        if let Ipv6(address) = dest.addr {
            if let Some(address) = address.to_ipv4_mapped() {
                return IpEndpoint::new(Ipv4(address), dest.port);
            }
        }
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

    fn try_recv_queued(&self, buf: &mut [u8], peek: bool) -> Option<inner::UdpReceived> {
        self.receive_queue.recv(buf, peek).map(
            |(copy_len, original_len, source, destination, ifindex, _)| inner::UdpReceived {
                copied: copy_len,
                original_len,
                source,
                destination,
                ifindex,
            },
        )
    }

    pub fn do_bind(
        &self,
        address: crate::net::socket::endpoint::InetEndpoint,
    ) -> Result<(), SystemError> {
        let endpoint = address.wire();
        super::common::port_policy::check_bind_access(&self.netns, endpoint, self.ip_version)?;
        if self.ip_version == IpVersion::Ipv4 && matches!(endpoint.addr, Ipv6(_)) {
            return Err(SystemError::EAFNOSUPPORT);
        }
        let scope =
            crate::net::socket::endpoint::needs_scope_id(address.addr).then_some(address.scope_id);
        let bind_iface = |index| {
            let device = if index == 0 {
                None
            } else {
                Some(
                    self.netns
                        .device_list()
                        .get(&index)
                        .cloned()
                        .ok_or(SystemError::ENODEV)?,
                )
            };
            if device.is_some()
                && !endpoint.addr.is_unspecified()
                && super::common::get_iface_for_local_bind(&endpoint.addr, &self.netns).is_none()
            {
                return Err(SystemError::EADDRNOTAVAIL);
            }
            super::common::explicit_bind_iface(endpoint.addr, &self.netns, device)
        };
        loop {
            {
                let inner = self.inner.read();
                match inner.as_ref() {
                    None => return Err(SystemError::EBADF),
                    Some(UdpInner::Bound(_)) => return Err(SystemError::EINVAL),
                    Some(UdpInner::Unbound(_)) => {}
                }
            }
            let candidate = scope
                .filter(|index| *index != 0)
                .map_or_else(|| self.device_binding.ifindex(), |index| index as usize);
            let target = bind_iface(candidate);
            let mut prepared = target
                .as_ref()
                .map_err(|error| error.clone())
                .and_then(|iface| self.prepare_iface_socket(iface.clone()));
            // Prepare/recover without socket guards. Keep all interface pins
            // outside the short device -> placement -> state transaction too.
            let current;
            let mut update = scope
                .map(|scope| self.device_binding.prepare_scope_bind(scope))
                .transpose()?;
            let _placement = self.iface_placement.write();
            let mut inner = self.inner.write();
            match inner.as_ref() {
                None => return Err(SystemError::EBADF),
                Some(UdpInner::Bound(_)) => return Err(SystemError::EINVAL),
                Some(UdpInner::Unbound(_)) => {}
            }
            let effective = update.as_ref().map_or_else(
                || self.device_binding.ifindex(),
                |update| update.target_ifindex(),
            );
            if effective != candidate {
                continue;
            }
            if let Some(update) = update.as_mut() {
                // State-race losers cannot change the winner's scope. Genuine
                // address/device failures retain the accepted numeric scope.
                update.commit();
            }
            current = bind_iface(effective)?;
            if !target
                .as_ref()
                .is_ok_and(|initial| Arc::ptr_eq(initial, &current))
            {
                continue;
            }
            let prepared = prepared.as_mut().map_err(|error| error.clone())?;
            let Some(UdpInner::Unbound(unbound)) = inner.take() else {
                unreachable!();
            };
            match unbound.bind_prepared(endpoint, self.bind_context(), true, prepared) {
                Ok(bound) => {
                    *inner = Some(UdpInner::Bound(bound));
                    return Ok(());
                }
                Err((unbound, error)) => {
                    *inner = Some(UdpInner::Unbound(unbound));
                    return Err(error);
                }
            }
        }
    }

    pub fn bind_ephemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        let fixed_address = {
            let inner = self.inner.read();
            match inner.as_ref() {
                Some(UdpInner::Bound(_)) => return Ok(()),
                None => return Err(SystemError::EBADF),
                Some(UdpInner::Unbound(unbound)) => unbound.bound_address(),
            }
        };
        let (required_oif, multicast_source) = output_flow::socket_constraints(self, remote)?;
        let flow = output_flow::resolve_ipv4_send_flow(
            &self.netns,
            IpListenEndpoint {
                addr: fixed_address,
                port: 0,
            },
            remote,
            required_oif,
            multicast_source,
        )?;
        let mut flow_target = flow
            .map(|flow| output_flow::ephemeral_target(self, flow))
            .transpose()?;
        if matches!(remote, IpAddress::Ipv6(_)) {
            let local = IpListenEndpoint {
                addr: fixed_address,
                port: 0,
            };
            let resolved = crate::net::route::resolve_ipv6_send_route(
                &self.netns,
                remote,
                required_oif,
                output_flow::bound_source_constraint(&self.netns, local),
            )?;
            flow_target = Some(EphemeralBindTarget {
                stack_owner: resolved.source_owner,
                // Autobind selects an inventory, not a permanent source.
                // Keep an explicit address lock retained across disconnect.
                local_addr: fixed_address
                    .unwrap_or(IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED)),
            });
        }
        let fixed_target = fixed_address
            .map(|address| {
                super::common::explicit_bind_iface(
                    address,
                    &self.netns,
                    self.device_binding.resolve_iface(&self.netns)?,
                )
                .map(|stack_owner| EphemeralBindTarget {
                    stack_owner,
                    local_addr: address,
                })
            })
            .transpose()?;
        let target = match fixed_target
            .or(flow_target)
            .or(self.device_ephemeral_bind_target(remote)?)
        {
            Some(target) => target,
            None => super::common::ephemeral_bind_target(remote, &self.netns, None)?,
        };
        let mut prepared = self.prepare_iface_socket(target.stack_owner)?;
        let _placement = self.iface_placement.write();
        let mut inner = self.inner.write();
        match inner.as_ref() {
            Some(UdpInner::Bound(_)) => return Ok(()),
            None => return Err(SystemError::EBADF),
            _ => {}
        }
        let Some(UdpInner::Unbound(unbound)) = inner.take() else {
            unreachable!();
        };
        match unbound.bind_prepared(
            smoltcp::wire::IpEndpoint::new(target.local_addr, 0),
            self.bind_context(),
            false,
            &mut prepared,
        ) {
            Ok(bound) => {
                *inner = Some(UdpInner::Bound(bound));
                Ok(())
            }
            Err((unbound, error)) => {
                *inner = Some(UdpInner::Unbound(unbound));
                Err(error)
            }
        }
    }

    pub fn is_bound(&self) -> bool {
        let inner = self.inner.read();
        if let Some(UdpInner::Bound(_)) = &*inner {
            return true;
        }
        return false;
    }

    /// Replace only the selected physical ring. Binding, peer and queued
    /// datagrams stay on the same protocol instance throughout the operation.
    fn resize_buffer(&self, receive: bool, size: usize) -> Result<(), SystemError> {
        let limit = size.checked_mul(2).ok_or(SystemError::ENOBUFS)?;
        // Match the supported socket inventory bound used by TCP. Permissions
        // and option parsing are checked by the caller before admission.
        if limit > 1 << 30 {
            return Err(SystemError::ENOBUFS);
        }
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        loop {
            let operation = self.resize_lock.lock();
            // IPv4 RX belongs to receive_queue, not the TX-only smoltcp entry.
            let protocol_rx = receive && self.ip_version == IpVersion::Ipv6;
            let capacity = {
                let mut inner = self.inner.write();
                let get = |socket: &mut inner::SmolUdpSocket| {
                    if receive {
                        socket.payload_recv_capacity()
                    } else {
                        socket.payload_send_capacity()
                    }
                };
                match inner.as_mut().ok_or(SystemError::EBADF)? {
                    UdpInner::Unbound(socket) => socket.with_mut_socket(get),
                    UdpInner::Bound(socket) => socket.with_mut_socket(get),
                }
            };
            let prepared = if (!receive || protocol_rx) && limit > capacity {
                crate::net::socket::buffer::try_owned_packet_buffer(
                    &self.memory,
                    inner::DEFAULT_METADATA_BUF_SIZE,
                    limit,
                )
                .map(Some)
            } else {
                Ok(None)
            };
            let mut candidate = match prepared {
                Ok(candidate) => candidate,
                Err(error) => {
                    drop(operation);
                    error.recover(&mut recovery)?;
                    continue;
                }
            };
            let mut became_writable = false;
            let committed = {
                let mut inner = self.inner.write();
                let commit = |socket: &mut inner::SmolUdpSocket| {
                    if receive && !protocol_rx {
                        return true;
                    }
                    let cap = if receive {
                        socket.payload_recv_capacity()
                    } else {
                        socket.payload_send_capacity()
                    };
                    if limit > cap {
                        let Some(candidate) = candidate.as_mut() else {
                            return false;
                        };
                        let swapped = if receive {
                            socket.swap_prepared_receive_buffer(candidate)
                        } else {
                            socket.swap_prepared_send_buffer(candidate)
                        };
                        if !swapped {
                            return false;
                        }
                    }
                    if receive {
                        socket.set_receive_buffer_limit(limit)
                    } else {
                        socket.set_send_buffer_limit(limit)
                    }
                };
                let committed = match inner.as_mut() {
                    Some(UdpInner::Unbound(socket)) => socket.with_mut_socket(commit),
                    Some(UdpInner::Bound(socket)) => socket.with_mut_socket(commit),
                    None => {
                        drop(inner);
                        drop(candidate);
                        drop(operation);
                        return Err(SystemError::EBADF);
                    }
                };
                if committed {
                    if receive {
                        self.recv_buf_size.store(size, Ordering::Release);
                        self.receive_queue.set_capacity(limit);
                    } else {
                        self.send_buf_size.store(size, Ordering::Release);
                        became_writable = self.send_account.set_limit_quiet(limit);
                    }
                }
                committed
            };
            // Both rejected candidates and successfully retired backing are
            // destroyed after inner/SocketSet guards, before quota recovery.
            drop(candidate);
            drop(operation);
            if committed {
                if !receive {
                    self.send_account.notify_limit_changed(became_writable);
                }
                return Ok(());
            }
        }
    }

    pub fn close(&self) {
        let mut retired_binding = None;
        let _placement = self.iface_placement.write();
        self.receive_queue.close();
        self.drop_ipv6_memberships();
        multicast_loopback::multicast_registry().unregister_all(&self.self_ref);
        crate::net::socket::inet::common::multicast::drop_ipv4_memberships(
            &self.netns,
            &self.ip_multicast_groups,
        );
        {
            let mut errors = self.errqueue.lock();
            errors.0 = 0;
            errors.1.clear();
        }
        let mut inner = self.inner.write();
        if let Some(UdpInner::Bound(bound)) = &mut *inner {
            self.netns
                .udp_bindings()
                .unbind(bound.endpoint().port, self.bind_id());
            retired_binding = Some(
                bound
                    .inner()
                    .retire_notification(self.self_ref.upgrade().unwrap()),
            );
            bound.close();
            inner.take();
        }
        // unbound socket just drop (only need to free memory)
    }

    fn disconnect_udp(&self) -> Result<(), SystemError> {
        let mut retired_binding = None;
        let mut device_clear = self.device_binding.prepare_clear();
        let _placement = self.iface_placement.write();
        let mut inner_guard = self.inner.write();
        let should_unbind = match inner_guard.as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.disconnect();
                self.receive_queue.disconnect();
                bound.update_receive_filter(0);
                bound.should_unbind_on_disconnect()
            }
            Some(UdpInner::Unbound(_)) => {
                device_clear.commit();
                return Ok(());
            }
            None => return Err(SystemError::EBADF),
        };

        if should_unbind {
            let Some(UdpInner::Bound(bound)) = inner_guard.take() else {
                unreachable!();
            };
            self.netns
                .udp_bindings()
                .unbind(bound.endpoint().port, self.bind_id());
            retired_binding = Some(
                bound
                    .inner()
                    .retire_notification(self.self_ref.upgrade().unwrap()),
            );
            inner_guard.replace(UdpInner::Unbound(bound.into_unbound_after_disconnect()));
            self.receive_queue.invalidate_binding();
        }
        device_clear.commit();
        Ok(())
    }

    pub fn try_recv(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<(usize, crate::net::socket::endpoint::InetEndpoint, usize), SystemError> {
        let received = self.try_recv_with_meta(buf, peek)?;
        Ok((
            received.copied,
            crate::net::socket::endpoint::InetEndpoint::from(received.source)
                .with_scope(received.ifindex as u32),
            received.original_len,
        ))
    }

    pub(super) fn try_recv_with_meta(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<inner::UdpReceived, SystemError> {
        self.take_socket_error()?;
        if let Some(received) = self.try_recv_queued(buf, peek) {
            return Ok(received);
        }
        match self.inner.write().as_mut() {
            Some(UdpInner::Bound(bound)) => bound.try_recv_with_metadata(buf, peek),
            Some(UdpInner::Unbound(unbound)) => unbound.try_recv_with_metadata(buf, peek),
            None => Err(SystemError::EBADF),
        }
    }

    fn build_udp_recv_cmsgs(
        &self,
        cmsg_buf: &mut CmsgBuffer,
        msg_flags: &mut i32,
        destination: IpEndpoint,
        ifindex: i32,
    ) -> Result<(), SystemError> {
        if self.ip_version != IpVersion::Ipv4 {
            return Ok(());
        }
        let (v4, local_port) = match destination {
            IpEndpoint {
                addr: smoltcp::wire::IpAddress::Ipv4(v4),
                port,
            } => (v4, port),
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
        // Can receive if there's data in the socket-wide queue or smoltcp queue.
        // OR if read is shutdown (shutdown should wake up recv() to return 0/EOF)
        if !self.receive_queue.is_empty() {
            return true;
        }
        if self.errqueue.lock().0 != 0 {
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
        if self.errqueue.lock().0 != 0 {
            return true;
        }
        let can_write = self.check_io_event().contains(EP::EPOLLOUT);
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let write_shutdown = (shutdown_bits & 0x02) != 0;
        can_write || write_shutdown
    }

    /// A blocking sender waits on the resource its selected path actually
    /// charges. In particular, a full prepared-output account must not spin
    /// merely because the legacy smoltcp queue still has room.
    fn can_send_to(
        &self,
        to: Option<crate::net::socket::endpoint::InetEndpoint>,
        payload_len: usize,
    ) -> bool {
        if self.errqueue.lock().0 != 0 || self.shutdown.load(Ordering::Acquire) & 0x02 != 0 {
            return true;
        }
        let destination = to.map(|endpoint| endpoint.wire()).or_else(|| {
            self.inner.read().as_ref().and_then(|inner| match inner {
                UdpInner::Bound(bound) => bound.remote_endpoint().ok(),
                _ => None,
            })
        });
        let Some(destination) = destination else {
            // Let the retry return EDESTADDRREQ instead of waiting for capacity.
            return true;
        };
        let length = match Self::normalize_unspecified_dest(destination).addr {
            Ipv4(_) => ipv4_packet::packet_len(payload_len),
            Ipv6(_) => ipv6_packet::packet_len(payload_len),
        };
        length.is_err() || length.is_ok_and(|bytes| self.send_account.can_charge(bytes))
    }

    #[inline]
    fn recv_return_len(copy_len: usize, orig_len: usize, flags: PMSG) -> usize {
        if flags.contains(PMSG::TRUNC) {
            orig_len
        } else {
            copy_len
        }
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

    fn pmtu_policy(&self, version: IpVersion) -> PmtuPolicy {
        let value = match version {
            IpVersion::Ipv4 => &self.pmtu_v4,
            IpVersion::Ipv6 => &self.pmtu_v6,
        }
        .load(Ordering::Acquire);
        PmtuPolicy::from_i32(value).unwrap_or_default()
    }

    fn take_socket_error(&self) -> Result<(), SystemError> {
        let errno = core::mem::take(&mut self.errqueue.lock().0);
        match SystemError::from_posix_errno(-errno) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn recv_error_enabled(&self, version: IpVersion) -> bool {
        match version {
            IpVersion::Ipv4 => self.recv_err_v4.load(Ordering::Acquire),
            IpVersion::Ipv6 => self.recv_err_v6.load(Ordering::Acquire),
        }
    }

    fn error_abi_address(&self, address: smoltcp::wire::IpAddress) -> smoltcp::wire::IpAddress {
        if self.ip_version == IpVersion::Ipv6 {
            if let Ipv4(address) = address {
                return Ipv6(address.to_ipv6_mapped());
            }
        }
        address
    }

    fn notify_error(&self) {
        self.wait_queue.wakeup_all(None);
        let _ = EventPoll::wakeup_epoll(self.epoll_items().as_ref(), EP::EPOLLERR);
    }

    fn enqueue_errqueue(&self, entry: ErrorQueueEntry) -> bool {
        let capacity = self.recv_buffer_size();
        let pushed = {
            let mut state = self.errqueue.lock();
            // LOCAL errors have already been returned synchronously. Queue
            // readiness is independent of the asynchronous socket error.
            state.1.push(entry, capacity)
        };
        if pushed {
            self.notify_error();
        }
        pushed
    }

    fn pop_errqueue(&self) -> Option<ErrorQueueEntry> {
        let mut queue = self.errqueue.lock();
        let entry = queue.1.pop()?;
        queue.0 = queue.1.pending_after_pop(&entry, queue.0);
        Some(entry)
    }

    fn enqueue_local_mtu_error(&self, mtu: usize, destination: IpEndpoint) {
        if !self.recv_error_enabled(destination.addr.version()) {
            return;
        }
        self.enqueue_errqueue(ErrorQueueEntry {
            error: SockExtendedErr {
                ee_errno: SystemError::EMSGSIZE.to_posix_errno().unsigned_abs(),
                ee_origin: SO_EE_ORIGIN_LOCAL,
                ee_info: mtu as u32,
                ..SockExtendedErr::default()
            },
            offender: None,
            destination: IpEndpoint::new(
                self.error_abi_address(destination.addr),
                destination.port,
            ),
            payload: Vec::new(),
            ipv6: self.ip_version == IpVersion::Ipv6,
            ingress_ifindex: 0,
            packet: None,
        });
    }

    /// Invoked after the binding table selects the reverse quoted tuple.
    /// No binding-table or device/protocol-stack lock is held here.
    fn handle_pmtu_feedback(&self, generation: u64, feedback: &crate::net::pmtu::PmtuFeedback) {
        let remote = IpEndpoint::new(feedback.destination, feedback.dst_port);
        let Some(connected) = self.ingress_match(generation, remote, feedback.source) else {
            return;
        };
        let version = feedback.source.version();
        let policy = self.pmtu_policy(version);
        // Unlike IPv4 UDP, IPv6 UDP suppresses the entire error handler
        // when the socket opts out of PMTU updates.
        if version == IpVersion::Ipv6 && !policy.accepts_updates() {
            return;
        }
        if policy.accepts_updates() {
            let oif = self.bound_device_ifindex();
            let _ = crate::net::route::pmtu::learn(
                &self.netns,
                feedback,
                (oif != 0).then_some(oif as u32),
            );
        }
        if version == IpVersion::Ipv4 && policy == PmtuPolicy::Dont {
            return;
        }
        let recverr = self.recv_error_enabled(version);
        let hard = policy != PmtuPolicy::Dont;
        if !recverr && (!connected || !hard) {
            return;
        }
        let errno = SystemError::EMSGSIZE.to_posix_errno().unsigned_abs();
        let mut error_entry = None;
        if recverr {
            let start = feedback.transport_offset.saturating_add(8);
            let quoted_payload = feedback.quote.get(start..).unwrap_or(&[]);
            let mut payload = Vec::new();
            if payload.try_reserve_exact(quoted_payload.len()).is_ok() {
                payload.extend_from_slice(quoted_payload);
                error_entry = Some(ErrorQueueEntry {
                    error: SockExtendedErr {
                        ee_errno: errno,
                        ee_origin: if version == IpVersion::Ipv6 {
                            SO_EE_ORIGIN_ICMP6
                        } else {
                            SO_EE_ORIGIN_ICMP
                        },
                        ee_type: if version == IpVersion::Ipv6 { 2 } else { 3 },
                        ee_code: if version == IpVersion::Ipv6 { 0 } else { 4 },
                        ee_info: feedback.mtu,
                        ..SockExtendedErr::default()
                    },
                    offender: Some(self.error_abi_address(feedback.offender)),
                    destination: IpEndpoint::new(self.error_abi_address(remote.addr), remote.port),
                    payload,
                    ipv6: self.ip_version == IpVersion::Ipv6,
                    ingress_ifindex: feedback.ingress_ifindex,
                    packet: Some(super::common::error_queue::ErrorPacketMetadata {
                        ttl: feedback.outer_ttl,
                        tos: feedback.outer_tos,
                        local_address: feedback.source,
                    }),
                });
            }
        }
        let capacity = self.recv_buffer_size();
        {
            let mut state = self.errqueue.lock();
            if let Some(entry) = error_entry {
                state.1.push(entry, capacity);
            }
            // Linux reports the socket error even when allocating or queuing
            // the extended error failed. Serialize it with ERRQUEUE consume.
            state.0 = errno as i32;
        }
        self.notify_error();
    }

    fn connected_path_mtu(&self, version: IpVersion) -> Result<usize, SystemError> {
        if version == IpVersion::Ipv4 {
            let snapshot = self.ipv4_send_snapshot(None).map_err(|error| {
                if error == SystemError::EDESTADDRREQ {
                    SystemError::ENOTCONN
                } else {
                    error
                }
            })?;
            return Ok(self
                .output_path_mtu(
                    snapshot.route,
                    IpEndpoint::new(Ipv4(snapshot.source), snapshot.source_port),
                    IpEndpoint::new(Ipv4(snapshot.destination), snapshot.destination_port),
                )?
                .path);
        }
        let (remote, local, connected_source) = {
            let inner = self.inner.read();
            let Some(UdpInner::Bound(bound)) = inner.as_ref() else {
                return Err(SystemError::ENOTCONN);
            };
            (
                bound.remote_endpoint()?,
                bound.endpoint(),
                bound.connected_source(),
            )
        };
        if remote.addr.version() == IpVersion::Ipv4 {
            // An IPv6 socket connected to a mapped peer has an IPv4 dst;
            // Linux IPV6_MTU reports that connected route's MTU as well.
            return self.connected_path_mtu(IpVersion::Ipv4);
        }
        let oif = self.bound_device_ifindex();
        let resolved = crate::net::route::resolve_ipv6_send_route(
            &self.netns,
            remote.addr,
            (oif != 0).then_some(oif as u32),
            local
                .addr
                .filter(|addr| !addr.is_unspecified())
                .or(connected_source),
        )?;
        Ok(self
            .output_path_mtu(
                resolved.decision,
                IpEndpoint::new(Ipv6(resolved.source), local.port),
                remote,
            )?
            .path)
    }

    /// Query the confirmed NAT path without executing rules or refreshing
    /// conntrack expiry. The socket's original tuple remains its ABI identity.
    fn output_path_mtu(
        &self,
        route: crate::net::route::OutputRouteDecision,
        local: IpEndpoint,
        remote: IpEndpoint,
    ) -> Result<crate::net::route::pmtu::PathMtu, SystemError> {
        let (source, destination) = crate::net::pmtu::routed_output_flow(
            &self.netns,
            local,
            remote,
            smoltcp::wire::IpProtocol::Udp,
        );
        let router = self.netns.router();
        let routes = crate::net::route::lock_output_routes(&router, self.netns.device_list());
        let route = routes
            .lookup_with_source(destination, route.required_oif, Some(source))
            .ok_or(SystemError::ENETUNREACH)?;
        Ok(crate::net::route::pmtu::path_mtu(
            &self.netns,
            route,
            source,
            destination,
        ))
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<crate::net::socket::endpoint::InetEndpoint>,
    ) -> Result<usize, SystemError> {
        self.take_socket_error()?;
        let scope_oif = to.and_then(|endpoint| endpoint.scope_oif());
        let to = to.map(|endpoint| endpoint.wire());
        // Validate the supplied family before mapped-v4 normalization. Only
        // AF_INET6 sockets may use an IPv6 sockaddr, including mapped addresses.
        if self.ip_version == IpVersion::Ipv4
            && to.is_some_and(|destination| matches!(destination.addr, Ipv6(_)))
        {
            return Err(SystemError::EAFNOSUPPORT);
        }
        if to.is_some_and(|destination| destination.port == 0) {
            return Err(SystemError::EINVAL);
        }
        let to = to.map(Self::normalize_unspecified_dest);
        let destination = to
            .or_else(|| {
                self.inner.read().as_ref().and_then(|inner| match inner {
                    UdpInner::Bound(bound) => bound.remote_endpoint().ok(),
                    _ => None,
                })
            })
            .ok_or(SystemError::EDESTADDRREQ)?;
        match Self::normalize_unspecified_dest(destination).addr {
            Ipv4(_) => self.try_send_ipv4_prepared(buf, to),
            Ipv6(_) => self.try_send_ipv6_prepared(buf, to, scope_oif),
        }
    }

    /// Snapshot only socket state under the placement lock. Both the nft
    /// interpreter and local receive handoff run after that lock is released.
    fn ipv4_send_snapshot(&self, to: Option<IpEndpoint>) -> Result<Ipv4SendSnapshot, SystemError> {
        let (destination, local, connected_source) = {
            let inner = self.inner.read();
            let bound = match inner.as_ref() {
                Some(UdpInner::Bound(bound)) => bound,
                Some(UdpInner::Unbound(_)) => return Err(SystemError::EDESTADDRREQ),
                None => return Err(SystemError::EBADF),
            };
            (
                Self::normalize_unspecified_dest(
                    to.or_else(|| bound.remote_endpoint().ok())
                        .ok_or(SystemError::EDESTADDRREQ)?,
                ),
                bound.endpoint(),
                bound.connected_source(),
            )
        };
        if to.is_some() && destination.port == 0 {
            return Err(SystemError::EINVAL);
        }
        let Ipv4(destination_addr) = destination.addr else {
            return Err(SystemError::EAFNOSUPPORT);
        };
        let (required_oif, multicast_source) =
            output_flow::socket_constraints(self, destination.addr)?;
        let resolved = output_flow::resolve_ipv4_send_route(
            &self.netns,
            local,
            destination.addr,
            required_oif,
            connected_source.or(multicast_source),
        )?;
        if resolved.decision.matched.kind == crate::net::route::RTN_BROADCAST
            && !self.so_broadcast.load(Ordering::Relaxed)
        {
            return Err(SystemError::EACCES);
        }
        let Ipv4(source) = resolved.source else {
            return Err(SystemError::EAFNOSUPPORT);
        };
        let owner =
            crate::net::socket::inet::common::get_iface_for_local_bind(&Ipv4(source), &self.netns)
                .ok_or(SystemError::ENETUNREACH)?;
        Ok(Ipv4SendSnapshot {
            owner,
            source,
            destination: destination_addr,
            source_port: local.port,
            destination_port: destination.port,
            route: resolved.output_decision(),
            ttl: if destination_addr.is_multicast() {
                self.ip_multicast_ttl.load(Ordering::Relaxed) as u8
            } else {
                64
            },
            multicast_loop: self.ip_multicast_loop.load(Ordering::Relaxed),
            checksum_enabled: !self.no_check.load(Ordering::Acquire),
        })
    }

    fn try_send_ipv4_prepared(
        &self,
        payload: &[u8],
        to: Option<IpEndpoint>,
    ) -> Result<usize, SystemError> {
        if payload.len() > u16::MAX as usize {
            return Err(SystemError::EMSGSIZE);
        }
        let packet_len = ipv4_packet::packet_len(payload.len());
        let placement = self.iface_placement.read();
        let ordinary_bound_send = {
            let inner = self.inner.read();
            match inner.as_ref() {
                Some(UdpInner::Bound(bound)) => to
                    .or_else(|| bound.remote_endpoint().ok())
                    .is_some_and(|dest| !dest.addr.is_multicast()),
                _ => false,
            }
        };
        let snapshot = if ordinary_bound_send {
            let snapshot = self.ipv4_send_snapshot(to)?;
            drop(placement);
            snapshot
        } else {
            drop(placement);
            loop {
                let unbound = matches!(self.inner.read().as_ref(), Some(UdpInner::Unbound(_)));
                if unbound {
                    let destination = to.ok_or(SystemError::EDESTADDRREQ)?;
                    if destination.port == 0 {
                        return Err(SystemError::EINVAL);
                    }
                    let Ipv4(_) = destination.addr else {
                        return Err(SystemError::EAFNOSUPPORT);
                    };
                    self.bind_ephemeral(Self::normalize_unspecified_dest(destination).addr)?;
                }
                // Binding performs its own pre-born transaction. Acquire the
                // send classification guard only after that operation returns.
                let _placement = self.iface_placement.write();
                if matches!(self.inner.read().as_ref(), Some(UdpInner::Unbound(_))) {
                    // Disconnect raced the lock-free preparation. No datagram
                    // or output reservation was published, so retry just bind.
                    continue;
                }
                break self.ipv4_send_snapshot(to)?;
            }
        };

        let policy = self.pmtu_policy(IpVersion::Ipv4);
        let path = self.output_path_mtu(
            snapshot.route,
            IpEndpoint::new(Ipv4(snapshot.source), snapshot.source_port),
            IpEndpoint::new(Ipv4(snapshot.destination), snapshot.destination_port),
        )?;
        let mtu = policy.effective_mtu(path.interface, path.path);
        let packet_len = packet_len.inspect_err(|_error| {
            self.enqueue_local_mtu_error(
                mtu,
                IpEndpoint::new(Ipv4(snapshot.destination), snapshot.destination_port),
            );
        })?;
        let dont_fragment = policy.ipv4_df(packet_len, mtu, path.locked);
        let identification = if dont_fragment {
            0
        } else {
            self.netns.next_ipv4_identification()
        };
        // Admission and the per-socket charge both precede any rule or clone
        // side effect. The charge follows the packet until output completion.
        let charge = self.send_account.charge(packet_len)?;
        let mut reservation = crate::driver::net::local_output::reserve_prepared_ip_output(
            snapshot.owner.as_ref(),
            &self.netns,
            packet_len,
            IpVersion::Ipv4,
        )?;
        ipv4_packet::emit(
            reservation.bytes_mut(),
            payload,
            snapshot.source,
            snapshot.destination,
            snapshot.source_port,
            snapshot.destination_port,
            snapshot.ttl,
            0,
            identification,
            dont_fragment,
            snapshot.checksum_enabled,
        )?;
        reservation.set_charge(charge);
        let result = crate::net::output::submit_prepared_ipv4_with_pmtu(
            &self.netns,
            reservation,
            snapshot.route,
            snapshot.multicast_loop,
            policy.allows_fragmentation(),
            policy,
        );
        let local_target = result.map_err(|error| {
            if error.error == SystemError::EMSGSIZE {
                if let Some(mtu) = error.mtu {
                    self.enqueue_local_mtu_error(
                        mtu,
                        IpEndpoint::new(Ipv4(snapshot.destination), snapshot.destination_port),
                    );
                }
            }
            error.error
        })?;
        if let Some(target) = local_target {
            // OUTPUT may reroute the packet. First advance the source owner's
            // admitted output, then the actual local delivery interface.
            // Neither the socket-placement nor FIB/ruleset locks are held.
            super::common::poll_local_output(&self.netns, &snapshot.owner, &target);
        }
        Ok(payload.len())
    }

    /// Admit native IPv6 unicast using one route/source snapshot. The socket
    /// placement lock protects endpoint/device state until that snapshot is
    /// captured, but is never held while executing nft rules or local input.
    fn try_send_ipv6_prepared(
        &self,
        payload: &[u8],
        to: Option<IpEndpoint>,
        scope_oif: Option<u32>,
    ) -> Result<usize, SystemError> {
        let packet_len = ipv6_packet::packet_len(payload.len());
        let placement = self.iface_placement.read();
        let (is_unbound, fixed_address) = match self.inner.read().as_ref() {
            Some(UdpInner::Unbound(unbound)) => (true, unbound.bound_address()),
            _ => (false, None),
        };
        let snapshot = if is_unbound {
            drop(placement);
            let destination =
                Self::normalize_unspecified_dest(to.ok_or(SystemError::EDESTADDRREQ)?);
            let Ipv6(destination_addr) = destination.addr else {
                return Err(SystemError::EAFNOSUPPORT);
            };
            if destination.port == 0 {
                return Err(SystemError::EINVAL);
            }
            let required_oif = match scope_oif {
                Some(index) => Some(index),
                None => output_flow::socket_constraints(self, destination.addr)?.0,
            };
            let receive_bind = IpListenEndpoint {
                addr: fixed_address,
                port: 0,
            };
            let resolved = crate::net::route::resolve_ipv6_send_route(
                &self.netns,
                destination.addr,
                required_oif,
                output_flow::bound_source_constraint(&self.netns, receive_bind),
            )?;
            let mut prepared = self.prepare_iface_socket(resolved.source_owner.clone())?;
            let _placement = self.iface_placement.write();
            let mut inner = self.inner.write();
            if !matches!(inner.as_ref(), Some(UdpInner::Unbound(_))) {
                return Err(SystemError::EBADF);
            }
            let Some(UdpInner::Unbound(unbound)) = inner.take() else {
                unreachable!();
            };
            let bound = match unbound.bind_prepared(
                // Implicit port allocation does not pin the source address.
                // A later send may select another IPv6 source after routing
                // changes, just as an unbound Linux UDP socket does.
                IpEndpoint::new(Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED), 0),
                self.bind_context(),
                false,
                &mut prepared,
            ) {
                Ok(bound) => bound,
                Err((unbound, error)) => {
                    *inner = Some(UdpInner::Unbound(unbound));
                    return Err(error);
                }
            };
            let owner = bound.inner().iface().clone();
            let source_port = bound.endpoint().port;
            *inner = Some(UdpInner::Bound(bound));
            Ipv6SendSnapshot {
                owner,
                route: resolved.decision,
                source: resolved.source,
                destination: destination_addr,
                source_port,
                destination_port: destination.port,
            }
        } else {
            let (destination, local, connected_source) = {
                let inner = self.inner.read();
                let bound = match inner.as_ref() {
                    Some(UdpInner::Bound(bound)) => bound,
                    Some(UdpInner::Unbound(_)) => unreachable!(),
                    None => return Err(SystemError::EBADF),
                };
                let destination = Self::normalize_unspecified_dest(
                    to.or_else(|| bound.remote_endpoint().ok())
                        .ok_or(SystemError::EDESTADDRREQ)?,
                );
                self.validate_bound_send_dest(bound, destination)?;
                (destination, bound.endpoint(), bound.connected_source())
            };
            let Ipv6(destination_addr) = destination.addr else {
                return Err(SystemError::EAFNOSUPPORT);
            };
            let required_oif = match scope_oif {
                Some(index) => Some(index),
                None => output_flow::socket_constraints(self, destination.addr)?.0,
            };
            let fixed_source =
                output_flow::bound_source_constraint(&self.netns, local).or(connected_source);
            let resolved = crate::net::route::resolve_ipv6_send_route(
                &self.netns,
                destination.addr,
                required_oif,
                fixed_source,
            )?;
            drop(placement);
            Ipv6SendSnapshot {
                owner: resolved.source_owner,
                route: resolved.decision,
                source: resolved.source,
                destination: destination_addr,
                source_port: local.port,
                destination_port: destination.port,
            }
        };

        let packet_len = packet_len.map_err(|error| {
            let policy = self.pmtu_policy(IpVersion::Ipv6);
            let path = match self.output_path_mtu(
                snapshot.route,
                IpEndpoint::new(Ipv6(snapshot.source), snapshot.source_port),
                IpEndpoint::new(Ipv6(snapshot.destination), snapshot.destination_port),
            ) {
                Ok(path) => path,
                Err(route_error) => return route_error,
            };
            let mut mtu = policy.effective_mtu(path.interface, path.path);
            let ceiling = self.ipv6_mtu.load(Ordering::Acquire);
            if ceiling != 0 {
                mtu = mtu.min(ceiling);
            }
            self.enqueue_local_mtu_error(
                mtu,
                IpEndpoint::new(Ipv6(snapshot.destination), snapshot.destination_port),
            );
            error
        })?;
        let charge = self.send_account.charge(packet_len)?;
        let mut reservation = crate::driver::net::local_output::reserve_prepared_ip_output(
            snapshot.owner.as_ref(),
            &self.netns,
            packet_len,
            IpVersion::Ipv6,
        )?;
        ipv6_packet::emit(
            reservation.bytes_mut(),
            payload,
            snapshot.source,
            snapshot.destination,
            snapshot.source_port,
            snapshot.destination_port,
            if snapshot.destination.is_multicast() {
                self.ipv6_multicast_hops.load(Ordering::Acquire) as u8
            } else {
                64
            },
        )?;
        reservation.set_charge(charge);
        let policy = self.pmtu_policy(IpVersion::Ipv6);
        let ceiling = self.ipv6_mtu.load(Ordering::Acquire);
        let result = crate::net::output::submit_prepared_ipv6_with_pmtu(
            &self.netns,
            reservation,
            snapshot.route,
            policy,
            (ceiling != 0).then_some(ceiling),
            self.ipv6_multicast_loop.load(Ordering::Acquire),
        );
        result.map_err(|error| {
            if error.error == SystemError::EMSGSIZE {
                if let Some(mtu) = error.mtu {
                    self.enqueue_local_mtu_error(
                        mtu,
                        IpEndpoint::new(Ipv6(snapshot.destination), snapshot.destination_port),
                    );
                }
            }
            error.error
        })?;
        Ok(payload.len())
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    /// Inject a loopback packet into this socket's receive buffer
    ///
    /// Returns true if the packet was successfully injected
    pub fn inject_loopback_packet(
        &self,
        generation: u64,
        src_endpoint: IpEndpoint,
        dst_addr: smoltcp::wire::IpAddress,
        dst_port: u16,
        ifindex: i32,
        payload: &[u8],
    ) -> bool {
        self.inject_ingress_packet(
            generation,
            src_endpoint,
            IpEndpoint::new(dst_addr, dst_port),
            ifindex,
            smoltcp::phy::PacketMeta::default(),
            payload,
        )
    }

    pub(crate) fn ingress_match(
        &self,
        generation: u64,
        source: IpEndpoint,
        destination: smoltcp::wire::IpAddress,
    ) -> Option<bool> {
        self.receive_queue
            .ingress_match(generation, source, destination)
    }

    pub(crate) fn inject_ingress_packet(
        &self,
        generation: u64,
        source: IpEndpoint,
        destination: IpEndpoint,
        ifindex: i32,
        meta: smoltcp::phy::PacketMeta,
        payload: &[u8],
    ) -> bool {
        if ifindex <= 0 || !self.device_binding.allows(ifindex as usize) {
            return false;
        }
        let Some(became_readable) =
            self.receive_queue
                .enqueue(generation, source, destination, ifindex, meta, payload)
        else {
            return false;
        };
        if became_readable {
            self.wait_queue.wakeup_all(None);
            let readable = EP::EPOLLIN | EP::EPOLLRDNORM;
            let _ = EventPoll::wakeup_epoll(self.epoll_items().as_ref(), readable);
            self.fasync_items
                .send_sigio(crate::filesystem::vfs::fasync::FASYNC_POLL_IN);
        }
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
