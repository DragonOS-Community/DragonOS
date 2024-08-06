use system_error::SystemError::{self, *};
use alloc::sync::Arc;
use crate::net::{Iface, NET_DEVICES};

pub mod port;
pub use port::PortManager;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SocketType {
    Raw,
    Icmp,
    Udp,
    Tcp,
    Dhcpv4,
    Dns,
}

#[derive(Debug)]
pub struct BoundInetInner {
    handle: smoltcp::iface::SocketHandle,
    iface: Arc<dyn Iface>,
    endpoint: smoltcp::wire::IpEndpoint,
    pub remote: Option<smoltcp::wire::IpEndpoint>,
}

impl BoundInetInner {
    pub fn bind<T>(
        socket: T, 
        socket_type: SocketType, 
        endpoint: smoltcp::wire::IpEndpoint
    ) -> Result<Self, SystemError>
    where 
        T: smoltcp::socket::AnySocket<'static>
    {
        let iface = get_iface_to_bind(&endpoint.addr).ok_or(ENODEV)?;
        iface.port_manager().bind_port(socket_type, endpoint.port)?;
        let handle = iface.sockets().lock_no_preempt().add(socket);
        Ok( Self { handle, iface, endpoint, remote: None } )
    }

    pub fn ephemeral_bind<T>(
        socket: T, 
        socket_type: SocketType, 
        remote_endpoint: smoltcp::wire::IpEndpoint
    ) -> Result<Self, SystemError>
    where 
        T: smoltcp::socket::AnySocket<'static>
    {
        let (iface, local_addr) = get_ephemeral_iface(&remote_endpoint.addr);
        let bound_port = iface.port_manager().bind_ephemeral_port(socket_type)?;
        let handle = iface.sockets().lock_no_preempt().add(socket);
        let endpoint = smoltcp::wire::IpEndpoint::new(local_addr, bound_port);
        Ok( Self { handle, iface, endpoint, remote: None } )
    }

    pub fn with_mut<T: smoltcp::socket::AnySocket<'static>, R, F: FnMut(&mut T) -> R>(&self, mut f: F) -> R {
        f(self.iface.sockets().lock().get_mut::<T>(self.handle))
    }

    pub fn endpoint(&self) -> smoltcp::wire::IpEndpoint {
        self.endpoint
    }

    pub fn iface(&self) -> &Arc<dyn Iface> {
        &self.iface
    }

    pub fn release(&self, socket_type: SocketType, port: u16) {
        self.iface.port_manager().unbind_port(socket_type, port);
        self.iface.sockets().lock_no_preempt().remove(self.handle);
    }
}

#[inline]
pub fn get_iface_to_bind(ip_addr: &smoltcp::wire::IpAddress) -> Option<Arc<dyn Iface>> {
    crate::net::NET_DEVICES
        .read_irqsave()
        .iter()
        .find(|(_, iface)| { 
            iface.inner_iface().lock().has_ip_addr(*ip_addr)
        })
        .map(|(_, iface)| iface.clone())
}

/// Get a suitable iface to deal with sendto/connect request if the socket is not bound to an iface.
/// If the remote address is the same as that of some iface, we will use the iface.
/// Otherwise, we will use a default interface.
fn get_ephemeral_iface(remote_ip_addr: &smoltcp::wire::IpAddress) -> (Arc<dyn Iface>, smoltcp::wire::IpAddress) {
    get_iface_to_bind(remote_ip_addr)
        .map(|iface| (iface, remote_ip_addr.clone()))
        .or({
            let ifaces = NET_DEVICES.read_irqsave();
            ifaces
                .iter()
                .find_map(|(_, iface)| {
                    iface.inner_iface().lock().ip_addrs().iter().find(|cidr| {
                        cidr.contains_addr(remote_ip_addr)
                    })
                    .map(|cidr| (iface.clone(), cidr.address()))
                })
        })
        .or({
            NET_DEVICES.read_irqsave().values().next()
                .map(|iface| (iface.clone(), iface.inner_iface().lock().ip_addrs()[0].address()))
        })
        .expect("No network interface")
}