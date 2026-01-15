use alloc::sync::Arc;

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
use super::loopback::register_raw_socket;
use super::options::RawSocketOptions;
use super::RawSocket;

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

        // Linux 语义：raw socket 创建时不要求必须存在网卡/路由。
        // 但为了让未 bind 的 raw socket 能接收数据包、且 poll/epoll 能正确唤醒，
        // 在存在 iface 时优先以“通配接收”方式附着到 loopback/默认 iface。
        // 若当前 netns 尚无可用 iface，则退化为 Unbound，允许后续 bind/connect/sendto 再完成选址与附着。
        let initial_inner = match UnboundRaw::new(ip_version, protocol).bind_wildcard(netns.clone())
        {
            Ok(wildcard) => RawInner::Wildcard(wildcard),
            Err(SystemError::ENODEV) => RawInner::Unbound(UnboundRaw::new(ip_version, protocol)),
            Err(e) => return Err(e),
        };

        let sock = Arc::new_cyclic(|me| Self {
            inner: crate::libs::rwsem::RwSem::new(Some(initial_inner)),
            options: crate::libs::rwsem::RwSem::new(options),
            nonblock: core::sync::atomic::AtomicBool::new(nonblock),
            wait_queue: crate::libs::wait_queue::WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: core::sync::atomic::AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: crate::net::socket::common::EPollItems::default(),
            fasync_items: crate::filesystem::vfs::fasync::FAsyncItems::default(),
            ip_version,
            protocol,
            loopback_rx: crate::libs::mutex::Mutex::new(super::loopback::LoopbackRxQueue::default()),
        });

        // Linux 语义：raw socket 未 bind 也应能被 poll/epoll 正确唤醒。
        // IfaceCommon::poll() 只会对注册在 bounds 列表里的 inet sockets 做 notify/wakeup，
        // 因此 wildcard 状态下需要注册到对应 iface。
        if let Some(RawInner::Wildcard(bound)) = sock.inner.read().as_ref() {
            bound.inner().iface().common().bind_socket(sock.clone());
        }

        register_raw_socket(&sock);

        Ok(sock)
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
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
        let mut inner = self.inner.write();
        let prev = inner.take().ok_or(SystemError::EINVAL)?;
        match prev {
            RawInner::Unbound(unbound) => match unbound.bind(local_addr, self.netns.clone()) {
                Ok(bound) => {
                    bound
                        .inner()
                        .iface()
                        .common()
                        .bind_socket(self.self_ref.upgrade().unwrap());
                    *inner = Some(RawInner::Bound(bound));
                    Ok(())
                }
                Err(e) => {
                    // bind 消费了 unbound（move）。失败时恢复为新的 Unbound 状态，
                    // 避免 inner=None 导致后续 unwrap panic。
                    *inner = Some(RawInner::Unbound(UnboundRaw::new(
                        self.ip_version,
                        self.protocol,
                    )));
                    // Linux 语义：绑定到不存在的本地地址应返回 EADDRNOTAVAIL。
                    Err(if matches!(e, SystemError::ENODEV) {
                        SystemError::EADDRNOTAVAIL
                    } else {
                        e
                    })
                }
            },
            RawInner::Wildcard(wildcard) => {
                // 从通配接收状态切换为用户显式绑定：先释放旧 handle，再按地址绑定。
                wildcard.close();
                let unbound = UnboundRaw::new(self.ip_version, self.protocol);
                match unbound.bind(local_addr, self.netns.clone()) {
                    Ok(bound) => {
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                        *inner = Some(RawInner::Bound(bound));
                        Ok(())
                    }
                    Err(e) => {
                        // 失败则回到通配接收（Linux 语义下不应让 socket 进入不可用状态）
                        let wildcard = UnboundRaw::new(self.ip_version, self.protocol)
                            .bind_wildcard(self.netns.clone())?;
                        *inner = Some(RawInner::Wildcard(wildcard));
                        Err(if matches!(e, SystemError::ENODEV) {
                            SystemError::EADDRNOTAVAIL
                        } else {
                            e
                        })
                    }
                }
            }
            other => {
                *inner = Some(other);
                Err(SystemError::EINVAL)
            }
        }
    }

    /// 绑定到临时地址（根据远程地址选择合适的本地地址）
    pub fn bind_ephemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        if !self.addr_matches_ip_version(remote) {
            return Err(SystemError::EAFNOSUPPORT);
        }
        let mut inner_guard = self.inner.write();
        let prev = inner_guard.take().ok_or(SystemError::EINVAL)?;
        match prev {
            RawInner::Bound(bound) => {
                inner_guard.replace(RawInner::Bound(bound));
                Ok(())
            }
            RawInner::Wildcard(wildcard) => {
                // Wildcard 仅表示已附着到某个 iface；为符合 Linux 语义（connect/getSockName），
                // 这里需要真正选址并记录 local_addr。
                wildcard.close();
                let unbound = UnboundRaw::new(self.ip_version, self.protocol);
                match unbound.bind_ephemeral(remote, self.netns.clone()) {
                    Ok(bound) => {
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                        inner_guard.replace(RawInner::Bound(bound));
                        Ok(())
                    }
                    Err(e) => {
                        // 失败则恢复为通配接收，避免 socket 进入不可用状态。
                        let wildcard = UnboundRaw::new(self.ip_version, self.protocol)
                            .bind_wildcard(self.netns.clone())?;
                        inner_guard.replace(RawInner::Wildcard(wildcard));
                        Err(e)
                    }
                }
            }
            RawInner::Unbound(unbound) => {
                match unbound.bind_ephemeral(remote, self.netns.clone()) {
                    Ok(bound) => {
                        bound
                            .inner()
                            .iface()
                            .common()
                            .bind_socket(self.self_ref.upgrade().unwrap());
                        inner_guard.replace(RawInner::Bound(bound));
                        Ok(())
                    }
                    Err(e) => {
                        // bind_ephemeral 消费了 unbound（move），失败恢复为 Unbound。
                        inner_guard.replace(RawInner::Unbound(UnboundRaw::new(
                            self.ip_version,
                            self.protocol,
                        )));
                        Err(e)
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
        let mut inner = self.inner.write();
        match &mut *inner {
            Some(RawInner::Bound(bound)) => {
                bound.close();
                inner.take();
            }
            Some(RawInner::Wildcard(bound)) => {
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
        self.check_io_event()
            .contains(crate::filesystem::epoll::EPollEventType::EPOLLIN)
    }

    #[inline]
    #[allow(dead_code)]
    pub fn can_send(&self) -> bool {
        self.check_io_event()
            .contains(crate::filesystem::epoll::EPollEventType::EPOLLOUT)
    }
}
