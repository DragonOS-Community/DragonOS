use alloc::sync::Arc;
use alloc::vec::Vec;

use smoltcp::wire::{IpAddress, IpProtocol, IpVersion};
use system_error::SystemError;

use crate::filesystem::vfs::vcore::generate_inode_id;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::Socket;
use crate::process::cred::CAPFlags;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;

use super::constants::ICMPV6_CHECKSUM_OFFSET;
use super::inner::{RawInner, UnboundRaw};
use super::loopback::{register_raw_socket, unregister_raw_socket};
use super::options::RawSocketOptions;
use super::{RawSocket, RawSocketData};

impl RawSocket {
    /// 根据 IP 版本返回 UNSPECIFIED 地址的端点
    pub(super) fn unspecified_endpoint(&self, proto: u16) -> Endpoint {
        let addr = match self.ip_version {
            IpVersion::Ipv4 => IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
            IpVersion::Ipv6 => IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED),
        };
        Endpoint::Ip(smoltcp::wire::IpEndpoint::new(addr, proto))
    }

    /// 创建新的 raw socket
    ///
    /// # 权限检查
    /// 需要 CAP_NET_RAW 权限
    pub fn new(
        ip_version: IpVersion,
        protocol: IpProtocol,
        nonblock: bool,
    ) -> Result<Arc<Self>, SystemError> {
        // CAP_NET_RAW 权限检查
        let cred = ProcessManager::current_pcb().cred();
        if !cred.has_capability(CAPFlags::CAP_NET_RAW) {
            log::warn!("RawSocket::new: CAP_NET_RAW check failed");
            return Err(SystemError::EPERM);
        }

        let netns = ProcessManager::current_netns();

        // IPPROTO_RAW (255) 自动启用 IP_HDRINCL
        let ip_hdrincl = protocol == IpProtocol::Unknown(255);

        let mut options = RawSocketOptions {
            ip_hdrincl,
            ..Default::default()
        };

        // Linux 语义：raw ICMPv6 socket 的 IPV6_CHECKSUM 固定为 icmp6_cksum 偏移。
        if ip_version == IpVersion::Ipv6 && protocol == IpProtocol::Icmpv6 {
            options.ipv6_checksum = ICMPV6_CHECKSUM_OFFSET;
        }
        let initial_sndbuf = options.sock_sndbuf as usize;

        // Linux 语义：raw socket 创建时不要求必须存在网卡/路由。
        // 但为了让未 bind 的 raw socket 能接收数据包、且 poll/epoll 能正确唤醒，
        // 在存在 iface 时优先以“通配接收”方式附着到 loopback/默认 iface。
        // 若当前 netns 尚无可用 iface，则退化为 Unbound，允许后续 bind/connect/sendto 再完成选址与附着。
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        let (memory, body, unbound, sock, me, send_account) =
            loop {
                let task = ProcessManager::current_pcb();
                let account = crate::cgroup::memory::account_for_task(&task);
                let creator = crate::cgroup::memory::object_for_task(&task);
                let prepared =
                    (|| {
                        let memory =
                            crate::net::socket::buffer::SocketMemory::prepare(&account, creator)?;
                        let body = crate::cgroup::memory::owned::OwnedBox::<RawSocketData>::prepare(
                    &memory.creator,
                    crate::cgroup::memory::accounting::KernelByteKind::SlabUnreclaimable,
                )?.try_allocate().map_err(|_| SystemError::ENOMEM)?;
                        let unbound = UnboundRaw::try_new(ip_version, protocol, &memory)?;
                        let sock = Arc::try_new(Self {
                            data: crate::libs::lazy_init::Lazy::new(),
                        })
                        .map_err(|_| SystemError::ENOMEM)?;
                        let me = Arc::downgrade(&sock);
                        let send_account = Arc::try_new(
                    crate::net::socket::inet::common::output_account::SocketOutputAccount::new(
                        me.clone(), initial_sndbuf,
                    ),
                ).map_err(|_| SystemError::ENOMEM)?;
                        Ok::<_, crate::mm::allocation::MemoryAllocationError>((
                            memory,
                            body,
                            unbound,
                            sock,
                            me,
                            send_account,
                        ))
                    })();
                match prepared {
                    Ok(prepared) => break prepared,
                    Err(error) => error.recover(&mut recovery)?,
                }
            };
        sock.data.init(
            body.init(RawSocketData {
                memory,
                inner: crate::libs::rwsem::RwSem::new(Some(RawInner::Unbound(unbound))),
                options: crate::libs::rwsem::RwSem::new(options),
                nonblock: core::sync::atomic::AtomicBool::new(nonblock),
                wait_queue: crate::libs::wait_queue::WaitQueue::default(),
                inode_id: generate_inode_id(),
                open_files: core::sync::atomic::AtomicUsize::new(0),
                fsnotify_watches: core::sync::atomic::AtomicUsize::new(0),
                self_ref: me.clone(),
                netns,
                device_binding: crate::net::socket::inet::common::SocketDeviceBinding::default(),
                send_account,
                epoll_items: crate::net::socket::common::EPollItems::default(),
                fasync_items: crate::filesystem::vfs::fasync::FAsyncItems::default(),
                ip_version,
                protocol,
                connected_port: core::sync::atomic::AtomicU32::new(0),
                loopback_rx: crate::libs::mutex::Mutex::new(
                    super::loopback::LoopbackRxQueue::default(),
                ),
                errors: crate::libs::mutex::Mutex::new((0, Default::default())),
                output_path: crate::libs::mutex::Mutex::new(None),
                ip_multicast_ifindex: core::sync::atomic::AtomicI32::new(0),
                ip_multicast_addr: core::sync::atomic::AtomicU32::new(0),
                ip_multicast_groups: crate::libs::mutex::Mutex::new(Vec::new()),
                ip_multicast_ttl: core::sync::atomic::AtomicI32::new(1),
                ip_multicast_loop: core::sync::atomic::AtomicBool::new(true),
                so_broadcast: core::sync::atomic::AtomicBool::new(false),
                send_timeout_us: core::sync::atomic::AtomicU64::new(u64::MAX),
                recv_timeout_us: core::sync::atomic::AtomicU64::new(u64::MAX),
            }),
        );

        // Linux 语义：raw socket 未 bind 也应能被 poll/epoll 正确唤醒。
        // IfaceCommon::poll() 只会对注册在 bounds 列表里的 inet sockets 做 notify/wakeup，
        // 因此 wildcard 状态下需要注册到对应 iface。
        let iface = sock
            .netns
            .loopback_iface()
            .map(|iface| iface as Arc<dyn crate::net::Iface>)
            .or_else(|| sock.netns.default_iface())
            .or_else(|| sock.netns.device_list().values().next().cloned());
        if let Some(iface) = iface {
            // A disappearing device is the same unattached state as having no
            // device at birth. Quota and real storage failures still propagate.
            match sock.prepare_iface_socket(iface) {
                Ok(mut prepared) => {
                    let mut inner = sock.inner.write();
                    let Some(RawInner::Unbound(unbound)) = inner.take() else {
                        unreachable!();
                    };
                    match unbound.bind_prepared(None, &mut prepared, sock.clone()) {
                        Ok(bound) => {
                            *inner = Some(RawInner::Wildcard(bound));
                        }
                        Err((unbound, SystemError::ENODEV)) => {
                            *inner = Some(RawInner::Unbound(unbound));
                        }
                        Err((unbound, error)) => {
                            *inner = Some(RawInner::Unbound(unbound));
                            return Err(error);
                        }
                    }
                }
                Err(SystemError::ENODEV) => {}
                Err(error) => return Err(error),
            }
        }

        register_raw_socket(&sock);

        Ok(sock)
    }

    fn prepare_iface_socket(
        &self,
        iface: Arc<dyn crate::net::Iface>,
    ) -> Result<super::super::common::registration::PreparedIfaceSocket, SystemError> {
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        loop {
            let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
            match super::super::common::registration::PreparedIfaceSocket::try_new(
                iface.clone(),
                self.netns.clone(),
                &creator,
            ) {
                Ok(prepared) => return Ok(prepared),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub(super) fn recv_timeout(&self) -> Option<crate::time::Duration> {
        let us = self
            .recv_timeout_us
            .load(core::sync::atomic::Ordering::Relaxed);
        if us == u64::MAX {
            None
        } else {
            Some(crate::time::Duration::from_micros(us))
        }
    }

    pub(super) fn send_timeout(&self) -> Option<crate::time::Duration> {
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
    pub(super) fn protocol_u16(&self) -> u16 {
        // Linux raw socket uses sockaddr_in{,6}.sin_port to carry the protocol number.
        // gVisor raw_socket_test expects this behavior.
        match self.protocol {
            IpProtocol::HopByHop => 0,
            IpProtocol::Icmp => 1,
            IpProtocol::Igmp => 2,
            IpProtocol::Tcp => 6,
            IpProtocol::Udp => 17,
            IpProtocol::Ipv6Route => 43,
            IpProtocol::Ipv6Frag => 44,
            IpProtocol::Icmpv6 => 58,
            IpProtocol::Ipv6NoNxt => 59,
            IpProtocol::Ipv6Opts => 60,
            IpProtocol::Unknown(v) => v as u16,
            _ => 0,
        }
    }

    #[inline]
    pub fn is_ipv6(&self) -> bool {
        self.ip_version == IpVersion::Ipv6
    }

    #[inline]
    pub(super) fn addr_matches_ip_version(&self, addr: smoltcp::wire::IpAddress) -> bool {
        matches!(
            (self.ip_version, addr),
            (IpVersion::Ipv4, smoltcp::wire::IpAddress::Ipv4(_))
                | (IpVersion::Ipv6, smoltcp::wire::IpAddress::Ipv6(_))
        )
    }

    /// 绑定到本地地址
    pub fn do_bind(&self, local_addr: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        if !self.addr_matches_ip_version(local_addr) {
            return Err(SystemError::EAFNOSUPPORT);
        }
        if !matches!(
            self.inner.read().as_ref(),
            Some(RawInner::Unbound(_) | RawInner::Wildcard(_))
        ) {
            return Err(SystemError::EINVAL);
        }
        let target = super::super::common::explicit_bind_iface(local_addr, &self.netns, None)?;
        self.bind_attachment(target, local_addr, false)
    }

    /// 绑定到临时地址（根据远程地址选择合适的本地地址）
    pub fn bind_ephemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        if !self.addr_matches_ip_version(remote) {
            return Err(SystemError::EAFNOSUPPORT);
        }
        if matches!(self.inner.read().as_ref(), Some(RawInner::Bound(_))) {
            return Ok(());
        }
        let target = super::super::common::ephemeral_bind_target(remote, &self.netns, None)?;
        self.bind_attachment(target.stack_owner, target.local_addr, true)
    }

    fn bind_attachment(
        &self,
        iface: Arc<dyn crate::net::Iface>,
        local: IpAddress,
        allow_bound: bool,
    ) -> Result<(), SystemError> {
        let mut prepared = self.prepare_iface_socket(iface)?;
        // The original vacant slots and Strong pin die after the inner guard.
        let mut retired = None;
        let mut inner = self.inner.write();
        match inner.as_mut().ok_or(SystemError::EINVAL)? {
            RawInner::Bound(_) => {
                if allow_bound {
                    Ok(())
                } else {
                    Err(SystemError::EINVAL)
                }
            }
            RawInner::Wildcard(bound) => {
                retired =
                    prepared.move_bound(bound.inner_mut(), self.self_ref.upgrade().unwrap())?;
                bound.set_local_addr(local);
                let Some(RawInner::Wildcard(bound)) = inner.take() else {
                    unreachable!()
                };
                *inner = Some(RawInner::Bound(bound));
                Ok(())
            }
            RawInner::Unbound(_) => {
                let Some(RawInner::Unbound(unbound)) = inner.take() else {
                    unreachable!()
                };
                match unbound.bind_prepared(
                    Some(local),
                    &mut prepared,
                    self.self_ref.upgrade().unwrap(),
                ) {
                    Ok(bound) => {
                        *inner = Some(RawInner::Bound(bound));
                        Ok(())
                    }
                    Err((unbound, error)) => {
                        *inner = Some(RawInner::Unbound(unbound));
                        Err(error)
                    }
                }
            }
        }
    }

    pub fn is_bound(&self) -> bool {
        let inner = self.inner.read();
        matches!(&*inner, Some(RawInner::Bound(_) | RawInner::Wildcard(_)))
    }

    pub fn close(&self) {
        if let Some(me) = self.self_ref.upgrade() {
            unregister_raw_socket(&me);
        }
        let mut retired = None;
        let mut inner = self.inner.write();
        match &mut *inner {
            Some(RawInner::Bound(bound)) => {
                retired = Some(
                    bound
                        .inner()
                        .retire_notification(self.self_ref.upgrade().unwrap()),
                );
                bound.close();
                inner.take();
            }
            Some(RawInner::Wildcard(bound)) => {
                retired = Some(
                    bound
                        .inner()
                        .retire_notification(self.self_ref.upgrade().unwrap()),
                );
                bound.close();
                inner.take();
            }
            _ => {}
        }
    }

    #[allow(dead_code)]
    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    /// 获取 IP 头长度
    pub(super) fn get_ip_header_len(&self, data: &[u8]) -> usize {
        match self.ip_version {
            IpVersion::Ipv4 => {
                if data.is_empty() {
                    return crate::net::socket::utils::IPV4_MIN_HEADER_LEN;
                }
                let ihl = (data[0] & 0x0F) as usize * 4;
                if ihl < crate::net::socket::utils::IPV4_MIN_HEADER_LEN {
                    crate::net::socket::utils::IPV4_MIN_HEADER_LEN
                } else {
                    ihl
                }
            }
            IpVersion::Ipv6 => crate::net::socket::utils::IPV6_HEADER_LEN,
        }
    }

    #[inline]
    pub fn can_recv(&self) -> bool {
        let pending_error = self.errors.lock().0 != 0;
        pending_error
            || self
                .check_io_event()
                .contains(crate::filesystem::epoll::EPollEventType::EPOLLIN)
    }

    #[inline]
    #[allow(dead_code)]
    pub fn can_send(&self) -> bool {
        self.check_io_event()
            .contains(crate::filesystem::epoll::EPollEventType::EPOLLOUT)
    }
}
