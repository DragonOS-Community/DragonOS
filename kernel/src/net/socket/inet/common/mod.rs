use crate::{
    driver::net::types::InterfaceFlags,
    net::{address::source_address_usable_on_iface, Iface},
    process::namespace::net_namespace::NetNamespace,
};
use alloc::sync::Arc;

pub mod port;
pub use port::PortManager;
mod device_binding;
pub use device_binding::{DeviceBindingUpdate, SocketDeviceBinding};
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

/// The interface-local smoltcp stack and address selected for an implicit
/// bind. `stack_owner` is deliberately not the route's output interface:
/// incoming packets are delivered to the owner of `local_addr`, while output
/// is independently routed through the namespace FIB.
pub(crate) struct EphemeralBindTarget {
    pub(crate) stack_owner: Arc<dyn Iface>,
    pub(crate) local_addr: smoltcp::wire::IpAddress,
}

/// Keeps namespace-routed polling active across a protocol-state publication
/// window. It is independent of the notification list so connect and close
/// can preserve the same output invariant without changing their lock order.
#[derive(Debug)]
pub(crate) struct RoutedSocketPublication {
    iface: Arc<dyn Iface>,
}

impl RoutedSocketPublication {
    pub(crate) fn begin(iface: Arc<dyn Iface>) -> Self {
        iface.common().begin_routed_socket_publication();
        Self { iface }
    }
}

impl Drop for RoutedSocketPublication {
    fn drop(&mut self) {
        self.iface.common().finish_routed_socket_publication();
    }
}

/// Returns whether a concrete bind address has a unique local-delivery owner.
/// Wildcard, multicast, and broadcast sockets remain interface-scoped and use
/// the device-selection policy instead.
#[inline]
pub(crate) fn bind_address_uses_local_owner(address: smoltcp::wire::IpAddress) -> bool {
    matches!(
        address,
        smoltcp::wire::IpAddress::Ipv4(address)
            if !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
    )
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
        Self::bind_recoverable(socket, address, netns).map_err(|(_, err)| err)
    }

    pub fn bind_recoverable<T>(
        socket: T,
        address: &smoltcp::wire::IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<Self, (T, SystemError)>
    where
        T: smoltcp::socket::AnySocket<'static>,
    {
        if address.is_unspecified() {
            let iface = match select_iface_for_unspecified(address, &netns) {
                Ok(iface) => iface,
                Err(err) => return Err((socket, err)),
            };
            let handle = iface.sockets().lock().add(socket);
            return Ok(Self {
                handle,
                iface,
                netns,
            });
        } else {
            let iface = match get_iface_for_local_bind(address, &netns) {
                Some(iface) => iface,
                None => return Err((socket, bind_addr_not_found_error(address, &netns))),
            };
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
        Self::bind_ephemeral_recoverable(socket, remote, netns).map_err(|(_, err)| err)
    }

    pub fn bind_ephemeral_recoverable<T>(
        socket: T,
        remote: smoltcp::wire::IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<(Self, smoltcp::wire::IpAddress), (T, SystemError)>
    where
        T: smoltcp::socket::AnySocket<'static>,
    {
        let target = match get_ephemeral_bind_target(&remote, netns.clone()) {
            Ok(result) => result,
            Err(err) => return Err((socket, err)),
        };
        // let bound_port = iface.port_manager().bind_ephemeral_port(socket_type)?;
        let handle = target.stack_owner.sockets().lock().add(socket);
        // let endpoint = smoltcp::wire::IpEndpoint::new(local_addr, bound_port);
        Ok((
            Self {
                handle,
                iface: target.stack_owner,
                netns,
            },
            target.local_addr,
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
        self.move_udp_to_iface_with(iface, || {})
    }

    /// Move a UDP socket between smoltcp interface socket sets. `detached` is
    /// invoked after removal from the old set and before publication in the new
    /// set, which gives callers one linearization point for related state.
    pub fn move_udp_to_iface_with<F>(
        &mut self,
        iface: Arc<dyn Iface>,
        detached: F,
    ) -> Result<(), SystemError>
    where
        F: FnOnce(),
    {
        if Arc::ptr_eq(&self.iface, &iface) {
            detached();
            return Ok(());
        }
        let socket = self.iface.sockets().lock().remove(self.handle);
        let smoltcp::socket::Socket::Udp(socket) = socket else {
            return Err(SystemError::EINVAL);
        };
        detached();
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

/// Validate the Linux dual-stack rule for an AF_INET6 socket that has already
/// been bound to a concrete local address.
///
/// Linux 6.6 keeps the effective local address family once an IPv6 socket is
/// bound to a specific address:
/// - bound to a native IPv6 address, then sending to an IPv4-mapped peer
///   returns `ENETUNREACH`
/// - bound to an IPv4-mapped address, then sending to a native IPv6 peer
///   returns `EAFNOSUPPORT`
///
/// Unspecified local addresses (`::`) remain dual-stack and therefore bypass
/// this check.
#[inline]
pub fn ensure_bound_dual_stack_remote_compatible(
    local_addr: smoltcp::wire::IpAddress,
    remote_addr: smoltcp::wire::IpAddress,
) -> Result<(), SystemError> {
    if local_addr.is_unspecified() {
        return Ok(());
    }

    match (local_addr, remote_addr) {
        (smoltcp::wire::IpAddress::Ipv6(_), smoltcp::wire::IpAddress::Ipv4(_)) => {
            Err(SystemError::ENETUNREACH)
        }
        (smoltcp::wire::IpAddress::Ipv4(_), smoltcp::wire::IpAddress::Ipv6(_)) => {
            Err(SystemError::EAFNOSUPPORT)
        }
        _ => Ok(()),
    }
}

/// Linux treats connect/send destinations of INADDR_ANY/IN6ADDR_ANY as
/// loopback in the same address family.
#[inline]
pub fn normalize_unspecified_endpoint_to_loopback(
    endpoint: smoltcp::wire::IpEndpoint,
) -> smoltcp::wire::IpEndpoint {
    if !endpoint.addr.is_unspecified() {
        return endpoint;
    }

    let addr = match endpoint.addr {
        smoltcp::wire::IpAddress::Ipv4(_) => smoltcp::wire::IpAddress::v4(127, 0, 0, 1),
        smoltcp::wire::IpAddress::Ipv6(_) => smoltcp::wire::IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1),
    };
    smoltcp::wire::IpEndpoint::new(addr, endpoint.port)
}

#[inline]
pub(crate) fn get_iface_for_local_bind(
    ip_addr: &smoltcp::wire::IpAddress,
    netns: &Arc<NetNamespace>,
) -> Option<Arc<dyn Iface>> {
    // Preserve existing Linux-compatible bind behavior for multicast and
    // broadcast addresses, where the address is not configured as a unicast
    // address on an interface.
    if ip_addr.is_multicast() || ip_addr.is_broadcast() {
        let device_list = netns.device_list();
        return netns
            .default_iface()
            .or_else(|| device_list.values().next().cloned());
    }

    if let Some(iface) = crate::net::route::local_address_owner(netns, *ip_addr) {
        return Some(iface);
    }

    // Linux treats IPv4 addresses in a loopback interface's configured subnet
    // as local for bind(2). IPv6 does not have this loopback-subnet exception:
    // binding to an unassigned IPv6 address in an on-link prefix fails with
    // EADDRNOTAVAIL.
    if let smoltcp::wire::IpAddress::Ipv4(v4_addr) = ip_addr {
        let device_list = netns.device_list();
        return device_list.iter().find_map(|(_, iface)| {
            loopback_iface_contains_v4(iface, *v4_addr).then(|| iface.clone())
        });
    }

    None
}

#[inline]
pub fn get_iface_to_bind(
    ip_addr: &smoltcp::wire::IpAddress,
    netns: Arc<NetNamespace>,
) -> Option<Arc<dyn Iface>> {
    let device_list = netns.device_list();

    // Subnet-directed broadcast should prefer the iface whose configured subnet matches.
    if let smoltcp::wire::IpAddress::Ipv4(target_broadcast) = ip_addr {
        if target_broadcast.is_broadcast() {
            if let Some(iface) = device_list.iter().find_map(|(_, iface)| {
                iface_matches_directed_broadcast(iface, *target_broadcast).then(|| iface.clone())
            }) {
                return Some(iface);
            }
        }
    }

    // For multicast/broadcast fallback, use default or first iface.
    if ip_addr.is_multicast() || ip_addr.is_broadcast() {
        return netns
            .default_iface()
            .or_else(|| device_list.values().next().cloned());
    }

    if let Some(iface) = device_list
        .iter()
        .find(|(_, iface)| iface.smol_iface().lock().has_ip_addr(*ip_addr))
        .map(|(_, iface)| iface.clone())
    {
        return Some(iface);
    }

    // Linux-like loopback behavior for IPv4: lo considers the whole configured subnet local.
    if let smoltcp::wire::IpAddress::Ipv4(v4_addr) = ip_addr {
        return device_list.iter().find_map(|(_, iface)| {
            loopback_iface_contains_v4(iface, *v4_addr).then(|| iface.clone())
        });
    }

    // IPv6 loopback destinations (::1 etc.) are delivered via the loopback interface.
    if let smoltcp::wire::IpAddress::Ipv6(v6_addr) = ip_addr {
        return device_list.iter().find_map(|(_, iface)| {
            loopback_iface_contains_v6(iface, *v6_addr).then(|| iface.clone())
        });
    }

    None
}

#[inline]
fn iface_matches_directed_broadcast(
    iface: &Arc<dyn Iface>,
    target_broadcast: smoltcp::wire::Ipv4Address,
) -> bool {
    let smol_iface = iface.smol_iface().lock();
    smol_iface.ip_addrs().iter().any(|cidr| match cidr {
        smoltcp::wire::IpCidr::Ipv4(v4_cidr) => {
            v4_cidr.broadcast().is_some_and(|b| b == target_broadcast)
        }
        _ => false,
    })
}

#[inline]
pub(super) fn loopback_iface_contains_v4(
    iface: &Arc<dyn Iface>,
    v4_addr: smoltcp::wire::Ipv4Address,
) -> bool {
    if !iface.flags().contains(InterfaceFlags::LOOPBACK) {
        return false;
    }
    let smol_iface = iface.smol_iface().lock();
    smol_iface.ip_addrs().iter().any(|cidr| match cidr {
        smoltcp::wire::IpCidr::Ipv4(v4_cidr) => v4_cidr.contains_addr(&v4_addr),
        _ => false,
    })
}

#[inline]
fn loopback_iface_contains_v6(iface: &Arc<dyn Iface>, v6_addr: smoltcp::wire::Ipv6Address) -> bool {
    if !iface.flags().contains(InterfaceFlags::LOOPBACK) {
        return false;
    }
    if v6_addr.is_loopback() {
        return true;
    }
    let smol_iface = iface.smol_iface().lock();
    smol_iface.ip_addrs().iter().any(|cidr| match cidr {
        smoltcp::wire::IpCidr::Ipv6(v6_cidr) => v6_cidr.contains_addr(&v6_addr),
        _ => false,
    })
}

/// Get a suitable iface to deal with sendto/connect request if the socket is not bound to an iface.
/// Linux-like behavior: for implicit bind on connect/sendto, the stack must be able to select a
/// valid local source address for the given remote destination.
fn get_ephemeral_bind_target(
    remote_ip_addr: &smoltcp::wire::IpAddress,
    netns: Arc<NetNamespace>,
) -> Result<EphemeralBindTarget, SystemError> {
    let no_source_error = no_source_addr_error(remote_ip_addr);
    let loopback_dst = is_loopback_destination(remote_ip_addr);

    // Unicast egress is decided exclusively by the authoritative per-netns
    // FIB. A miss must not fall back to default_iface or device iteration,
    // otherwise RTM_GETROUTE and the actual packet path disagree.
    if !remote_ip_addr.is_unspecified()
        && !remote_ip_addr.is_multicast()
        && !remote_ip_addr.is_broadcast()
    {
        if matches!(remote_ip_addr, smoltcp::wire::IpAddress::Ipv4(_)) {
            let resolved =
                crate::net::route::resolve_ipv4_route(&netns, *remote_ip_addr, None, None)?;
            let iface = netns
                .device_list()
                .get(&(resolved.decision.oif as usize))
                .cloned()
                .ok_or(SystemError::ENETUNREACH)?;
            return ephemeral_target_for_source(&netns, iface, resolved.source, no_source_error);
        }
        let decision = crate::net::route::lookup(&netns, *remote_ip_addr)
            .ok_or_else(|| no_source_error.clone())?;
        let iface = netns
            .device_list()
            .get(&(decision.oif as usize))
            .cloned()
            .ok_or(SystemError::ENETUNREACH)?;
        ensure_iface_up_for_route(&iface, decision.matched.kind)?;
        // An explicit FIB decision is authoritative, including routes that
        // deliberately send non-loopback destinations through `lo`.
        let source = ipv6_route_source_from_decision(&netns, &iface, remote_ip_addr, decision)?;
        return ephemeral_target_for_source(&netns, iface, source, no_source_error);
    }

    let default_iface = netns.default_iface();

    if let Some(iface) = get_iface_to_bind(remote_ip_addr, netns.clone()) {
        if iface_allowed_for_remote(&iface, loopback_dst) {
            if let Some(local_addr) = pick_configured_source_addr(&iface, remote_ip_addr) {
                return ephemeral_target_for_source(&netns, iface, local_addr, no_source_error);
            }
        }
    }

    if let Some(iface) = default_iface {
        if iface_allowed_for_remote(&iface, loopback_dst) {
            if let Some(local_addr) = pick_configured_source_addr(&iface, remote_ip_addr) {
                return ephemeral_target_for_source(&netns, iface, local_addr, no_source_error);
            }
        }
    }

    // Clone the candidate before owner resolution: local_address_owner() also
    // reads the topology, and RwSem writer preference forbids recursive reads
    // once a writer is queued.
    let (candidate, no_devices) = {
        let devices = netns.device_list();
        let candidate = devices.iter().find_map(|(_, iface)| {
            if !iface_allowed_for_remote(iface, loopback_dst) {
                return None;
            }
            pick_configured_source_addr(iface, remote_ip_addr)
                .map(|local_addr| (iface.clone(), local_addr))
        });
        (candidate, devices.is_empty())
    };
    if let Some((iface, local_addr)) = candidate {
        return ephemeral_target_for_source(&netns, iface, local_addr, no_source_error);
    }

    if no_devices {
        return Err(SystemError::ENODEV);
    }

    Err(no_source_error)
}

/// Selects the SocketSet owner for a source chosen on `egress_iface`.
///
/// IPv4 local delivery follows the namespace local FIB and may therefore
/// target a different interface from egress. Native IPv6 output is still
/// interface-local, so IPv6 retains the egress stack until the routed-output
/// backend supports that family as well.
fn ephemeral_target_for_source(
    netns: &Arc<NetNamespace>,
    egress_iface: Arc<dyn Iface>,
    local_addr: smoltcp::wire::IpAddress,
    no_source_error: SystemError,
) -> Result<EphemeralBindTarget, SystemError> {
    let stack_owner = match local_addr {
        smoltcp::wire::IpAddress::Ipv4(address) if !address.is_unspecified() => {
            crate::net::route::local_address_owner(netns, local_addr).ok_or(no_source_error)?
        }
        _ => egress_iface,
    };
    Ok(EphemeralBindTarget {
        stack_owner,
        local_addr,
    })
}

pub(crate) fn ephemeral_bind_target_on_iface(
    netns: &Arc<NetNamespace>,
    egress_iface: Arc<dyn Iface>,
    remote: &smoltcp::wire::IpAddress,
) -> Result<EphemeralBindTarget, SystemError> {
    let local_addr = route_source_on_iface(netns, &egress_iface, remote)?;
    ephemeral_target_for_source(
        netns,
        egress_iface,
        local_addr,
        no_source_addr_error(remote),
    )
}

pub(crate) fn route_source_on_iface(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    remote: &smoltcp::wire::IpAddress,
) -> Result<smoltcp::wire::IpAddress, SystemError> {
    if remote.is_unspecified() || remote.is_multicast() || remote.is_broadcast() {
        ensure_iface_up(iface)?;
        return pick_configured_source_addr(iface, remote).ok_or(no_source_addr_error(remote));
    }
    if matches!(remote, smoltcp::wire::IpAddress::Ipv4(_)) {
        return crate::net::route::resolve_ipv4_route(
            netns,
            *remote,
            Some(iface.nic_id() as u32),
            None,
        )
        .map(|resolved| resolved.source);
    }
    let decision = crate::net::route::lookup_on_iface(netns, *remote, iface.nic_id() as u32)
        .ok_or(SystemError::ENETUNREACH)?;
    ensure_iface_up_for_route(iface, decision.matched.kind)?;
    ipv6_route_source_from_decision(netns, iface, remote, decision)
}

fn ensure_iface_up_for_route(iface: &Arc<dyn Iface>, kind: u8) -> Result<(), SystemError> {
    if kind == crate::net::route::RTN_LOCAL {
        return Ok(());
    }
    ensure_iface_up(iface)
}

fn ipv6_route_source_from_decision(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    remote: &smoltcp::wire::IpAddress,
    decision: crate::net::route::RouteLookupResult,
) -> Result<smoltcp::wire::IpAddress, SystemError> {
    debug_assert_eq!(decision.oif as usize, iface.nic_id());
    debug_assert!(matches!(remote, smoltcp::wire::IpAddress::Ipv6(_)));
    match decision.source {
        crate::net::route::RouteSourcePolicy::Preferred(source)
            if source_address_usable_on_iface(netns, iface, source) =>
        {
            Ok(source)
        }
        crate::net::route::RouteSourcePolicy::Preferred(_) => Err(no_source_addr_error(remote)),
        crate::net::route::RouteSourcePolicy::SelectConfigured => {
            pick_configured_source_addr(iface, remote).ok_or(no_source_addr_error(remote))
        }
        crate::net::route::RouteSourcePolicy::AllowUnspecified => {
            Ok(pick_configured_source_addr(iface, remote)
                .unwrap_or(smoltcp::wire::IpAddress::v4(0, 0, 0, 0)))
        }
    }
}

fn ensure_iface_up(iface: &Arc<dyn Iface>) -> Result<(), SystemError> {
    if iface.flags().contains(InterfaceFlags::UP) {
        Ok(())
    } else {
        Err(SystemError::ENETDOWN)
    }
}

fn iface_allowed_for_remote(iface: &Arc<dyn Iface>, loopback_dst: bool) -> bool {
    let flags = iface.flags();
    flags.contains(InterfaceFlags::UP) && flags.contains(InterfaceFlags::LOOPBACK) == loopback_dst
}

fn no_source_addr_error(remote_ip_addr: &smoltcp::wire::IpAddress) -> SystemError {
    // gVisor socket_ip_unbound_netlink / Linux 6.6:
    // - IPv6 loopback destination (::1) without a local source -> EADDRNOTAVAIL
    // - All other no-source cases (incl. IPv4 loopback with no lo route) -> ENETUNREACH
    match remote_ip_addr {
        smoltcp::wire::IpAddress::Ipv4(_) => SystemError::ENETUNREACH,
        // Linux IPv6 connect() distinguishes "no route to a remote network"
        // from "the local/loopback destination exists but no source address can
        // be selected".  After ::1 is removed from lo, connecting to ::1 fails
        // from ipv6_get_saddr() with EADDRNOTAVAIL.
        smoltcp::wire::IpAddress::Ipv6(addr) if addr.is_loopback() => SystemError::EADDRNOTAVAIL,
        smoltcp::wire::IpAddress::Ipv6(_) => SystemError::ENETUNREACH,
    }
}

pub(crate) fn pick_configured_source_addr(
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

fn bind_addr_not_found_error(
    addr: &smoltcp::wire::IpAddress,
    netns: &Arc<NetNamespace>,
) -> SystemError {
    if netns.device_list().is_empty() {
        return SystemError::ENODEV;
    }

    match addr {
        smoltcp::wire::IpAddress::Ipv4(_) | smoltcp::wire::IpAddress::Ipv6(_) => {
            SystemError::EADDRNOTAVAIL
        }
    }
}

/// Select a suitable network interface for binding to an unspecified address.
///
/// Selection logic (in priority order):
/// 1. Use the explicitly set default interface
/// 2. Find an interface with a matching address family (IPv6 socket -> interface with IPv6 address)
/// 3. Fall back to the first available interface
pub(crate) fn select_iface_for_unspecified(
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
