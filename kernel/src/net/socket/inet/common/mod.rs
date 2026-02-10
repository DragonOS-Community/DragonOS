use crate::{
    driver::net::types::InterfaceFlags, net::Iface, process::namespace::net_namespace::NetNamespace,
};
use alloc::sync::Arc;

pub mod port;
pub use port::PortManager;
pub mod multicast;
pub use multicast::{apply_ipv4_membership, apply_ipv4_multicast_if, Ipv4MulticastMembership};
use system_error::SystemError;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Types {
    Raw,
    Icmp,
    Udp,
    Tcp,
    Dhcpv4,
    Dns,
}

/**
 * 目前，以下设计仍然没有考虑多网卡的listen问题，仅只解决了socket在绑定单网卡下的问题。
 */

#[derive(Debug)]
pub struct BoundInner {
    handle: smoltcp::iface::SocketHandle,
    iface: Arc<dyn Iface>,
    netns: Arc<NetNamespace>,
    // inner: Vec<(smoltcp::iface::SocketHandle, Arc<dyn Iface>)>
    // address: smoltcp::wire::IpAddress,
}

impl BoundInner {
    /// # `bind`
    /// 将socket绑定到指定的地址上，置入指定的网络接口中
    pub fn bind<T>(
        socket: T,
        // socket_type: Types,
        address: &smoltcp::wire::IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<Self, SystemError>
    where
        T: smoltcp::socket::AnySocket<'static>,
    {
        if address.is_unspecified() {
            let iface = select_iface_for_unspecified(address, &netns)?;
            let handle = iface.sockets().lock().add(socket);
            return Ok(Self {
                handle,
                iface,
                netns,
            });
        } else {
            let iface = get_iface_to_bind(address, netns.clone()).ok_or(SystemError::ENODEV)?;
            // log::debug!(
            //     "BoundInner::bind: binding to iface {} for address {:?}",
            //     iface.iface_name(),
            //     address
            // );
            let handle = iface.sockets().lock().add(socket);
            return Ok(Self {
                handle,
                iface,
                netns,
            });
        }
    }

    /// Bind a socket to a specific iface without selecting by address.
    ///
    /// This is useful for sockets that conceptually listen on all local addresses
    /// (e.g., unbound raw sockets) but still need to be attached to an iface so
    /// that packets can be delivered.
    pub fn bind_on_iface<T>(
        socket: T,
        iface: Arc<dyn Iface>,
        netns: Arc<NetNamespace>,
    ) -> Result<Self, SystemError>
    where
        T: smoltcp::socket::AnySocket<'static>,
    {
        let handle = iface.sockets().lock().add(socket);
        Ok(Self {
            handle,
            iface,
            netns,
        })
    }

    pub fn bind_ephemeral<T>(
        socket: T,
        // socket_type: Types,
        remote: smoltcp::wire::IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<(Self, smoltcp::wire::IpAddress), SystemError>
    where
        T: smoltcp::socket::AnySocket<'static>,
    {
        let (iface, address) = get_ephemeral_iface(&remote, netns.clone())?;
        // let bound_port = iface.port_manager().bind_ephemeral_port(socket_type)?;
        let handle = iface.sockets().lock().add(socket);
        // let endpoint = smoltcp::wire::IpEndpoint::new(local_addr, bound_port);
        Ok((
            Self {
                handle,
                iface,
                netns,
            },
            address,
        ))
    }

    pub fn port_manager(&self) -> &PortManager {
        self.iface.port_manager()
    }

    pub fn with_mut<T: smoltcp::socket::AnySocket<'static>, R, F: FnMut(&mut T) -> R>(
        &self,
        mut f: F,
    ) -> R {
        f(self.iface.sockets().lock().get_mut::<T>(self.handle))
    }

    pub fn with<T: smoltcp::socket::AnySocket<'static>, R, F: Fn(&T) -> R>(&self, f: F) -> R {
        f(self.iface.sockets().lock().get::<T>(self.handle))
    }

    pub fn iface(&self) -> &Arc<dyn Iface> {
        &self.iface
    }

    pub fn move_udp_to_iface(&mut self, iface: Arc<dyn Iface>) -> Result<(), SystemError> {
        if Arc::ptr_eq(&self.iface, &iface) {
            return Ok(());
        }
        let socket = self.iface.sockets().lock().remove(self.handle);
        let smoltcp::socket::Socket::Udp(socket) = socket else {
            return Err(SystemError::EINVAL);
        };
        let handle = iface.sockets().lock().add(socket);
        self.iface = iface;
        self.handle = handle;
        Ok(())
    }

    #[inline]
    pub fn handle(&self) -> smoltcp::iface::SocketHandle {
        self.handle
    }

    pub fn release(&self) {
        self.iface.sockets().lock().remove(self.handle);
    }

    pub fn into_socket(self) -> smoltcp::socket::Socket<'static> {
        self.iface.sockets().lock().remove(self.handle)
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }
}

#[inline]
pub fn get_iface_to_bind(
    ip_addr: &smoltcp::wire::IpAddress,
    netns: Arc<NetNamespace>,
) -> Option<Arc<dyn Iface>> {
    // For multicast or broadcast addresses, use the default interface or first available
    // Linux allows binding to these addresses for filtering purposes
    if ip_addr.is_multicast() || ip_addr.is_broadcast() {
        return netns
            .default_iface()
            .or_else(|| netns.device_list().values().next().cloned());
    }

    netns
        .device_list()
        .iter()
        .find(|(_, iface)| iface.smol_iface().lock().has_ip_addr(*ip_addr))
        .map(|(_, iface)| iface.clone())
}

/// Get a suitable iface to deal with sendto/connect request if the socket is not bound to an iface.
/// Linux-like behavior: for implicit bind on connect/sendto, the stack must be able to select a
/// valid local source address for the given remote destination.
fn get_ephemeral_iface(
    remote_ip_addr: &smoltcp::wire::IpAddress,
    netns: Arc<NetNamespace>,
) -> Result<(Arc<dyn Iface>, smoltcp::wire::IpAddress), SystemError> {
    let default_iface = netns.default_iface();
    let no_source_error = no_source_addr_error(remote_ip_addr);
    let loopback_dst = is_loopback_destination(remote_ip_addr);

    if let Some(iface) = get_iface_to_bind(remote_ip_addr, netns.clone()) {
        if !loopback_dst || iface.flags().contains(InterfaceFlags::LOOPBACK) {
            if let Some(local_addr) = pick_configured_source_addr(&iface, remote_ip_addr) {
                return Ok((iface, local_addr));
            }
        }
    }

    if let Some(iface) = default_iface {
        if !loopback_dst || iface.flags().contains(InterfaceFlags::LOOPBACK) {
            if let Some(local_addr) = pick_configured_source_addr(&iface, remote_ip_addr) {
                return Ok((iface, local_addr));
            }
        }
    }

    for (_, iface) in netns.device_list().iter() {
        if loopback_dst && !iface.flags().contains(InterfaceFlags::LOOPBACK) {
            continue;
        }

        if let Some(local_addr) = pick_configured_source_addr(iface, remote_ip_addr) {
            return Ok((iface.clone(), local_addr));
        }
    }

    if netns.device_list().is_empty() {
        return Err(SystemError::ENODEV);
    }

    Err(no_source_error)
}

fn no_source_addr_error(remote_ip_addr: &smoltcp::wire::IpAddress) -> SystemError {
    match remote_ip_addr {
        smoltcp::wire::IpAddress::Ipv4(_) => SystemError::ENETUNREACH,
        smoltcp::wire::IpAddress::Ipv6(_) => SystemError::EADDRNOTAVAIL,
    }
}

fn pick_configured_source_addr(
    iface: &Arc<dyn Iface>,
    remote_ip_addr: &smoltcp::wire::IpAddress,
) -> Option<smoltcp::wire::IpAddress> {
    let smol_iface = iface.smol_iface().lock();

    if remote_ip_addr.is_unspecified() {
        return smol_iface.ip_addrs().iter().find_map(|cidr| {
            let addr = cidr.address();
            match (remote_ip_addr, addr) {
                (smoltcp::wire::IpAddress::Ipv4(_), smoltcp::wire::IpAddress::Ipv4(_))
                | (smoltcp::wire::IpAddress::Ipv6(_), smoltcp::wire::IpAddress::Ipv6(_)) => {
                    Some(addr)
                }
                _ => None,
            }
        });
    }

    let selected = smol_iface.get_source_address(remote_ip_addr);
    let selected_is_configured = selected
        .as_ref()
        .map(|addr| smol_iface.has_ip_addr(*addr))
        .unwrap_or(false);

    selected.filter(|_| selected_is_configured)
}

fn is_loopback_destination(remote_ip_addr: &smoltcp::wire::IpAddress) -> bool {
    match remote_ip_addr {
        smoltcp::wire::IpAddress::Ipv4(addr) => addr.is_loopback(),
        smoltcp::wire::IpAddress::Ipv6(addr) => addr.is_loopback(),
    }
}

/// Select a suitable network interface for binding to an unspecified address.
///
/// Selection logic (in priority order):
/// 1. Use the explicitly set default interface
/// 2. Find an interface with a matching address family (IPv6 socket -> interface with IPv6 address)
/// 3. Fall back to the first available interface
fn select_iface_for_unspecified(
    address: &smoltcp::wire::IpAddress,
    netns: &Arc<NetNamespace>,
) -> Result<Arc<dyn Iface>, SystemError> {
    // 1. Prefer explicitly configured default interface
    if let Some(iface) = netns.default_iface() {
        return Ok(iface);
    }

    // 2. Find interface with matching address family
    let device_list = netns.device_list();
    for (_nic_id, iface) in device_list.iter() {
        let smol_iface = iface.smol_iface().lock();
        let has_matching_family = smol_iface.ip_addrs().iter().any(|cidr| {
            matches!(
                (address, cidr.address()),
                (
                    smoltcp::wire::IpAddress::Ipv6(_),
                    smoltcp::wire::IpAddress::Ipv6(_)
                ) | (
                    smoltcp::wire::IpAddress::Ipv4(_),
                    smoltcp::wire::IpAddress::Ipv4(_)
                )
            )
        });
        if has_matching_family {
            return Ok(iface.clone());
        }
    }

    // 3. Fall back to first available interface
    device_list
        .values()
        .next()
        .cloned()
        .ok_or(SystemError::ENODEV)
}
