use crate::{net::Iface, process::namespace::net_namespace::NetNamespace};
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
        let (iface, address) = get_ephemeral_iface(&remote, netns.clone());
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
    netns
        .device_list()
        .iter()
        .find(|(_, iface)| iface.smol_iface().lock().has_ip_addr(*ip_addr))
        .map(|(_, iface)| iface.clone())
}

/// Get a suitable iface to deal with sendto/connect request if the socket is not bound to an iface.
/// If the remote address is the same as that of some iface, we will use the iface.
/// Otherwise, we will use a default interface.
fn get_ephemeral_iface(
    remote_ip_addr: &smoltcp::wire::IpAddress,
    netns: Arc<NetNamespace>,
) -> (Arc<dyn Iface>, smoltcp::wire::IpAddress) {
    get_iface_to_bind(remote_ip_addr, netns.clone())
        .map(|iface| (iface, *remote_ip_addr))
        .or({
            let ifaces = netns.device_list();
            ifaces.iter().find_map(|(_, iface)| {
                iface
                    .smol_iface()
                    .lock()
                    .ip_addrs()
                    .iter()
                    .find(|cidr| cidr.contains_addr(remote_ip_addr))
                    .map(|cidr| (iface.clone(), cidr.address()))
            })
        })
        .or({
            netns.device_list().values().next().map(|iface| {
                (
                    iface.clone(),
                    iface.smol_iface().lock().ip_addrs()[0].address(),
                )
            })
        })
        .expect("No network interface")
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
