use crate::net::{Iface, NET_DEVICES};
use alloc::sync::Arc;
use system_error::SystemError::{self, *};

pub mod port;
pub use port::PortManager;

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
    ) -> Result<Self, SystemError>
    where
        T: smoltcp::socket::AnySocket<'static>,
    {
        if address.is_unspecified() {
            // 强绑VirtualIO
            let iface = NET_DEVICES
                .read_irqsave()
                .iter()
                .find_map(|(_, v)| {
                    if v.common().is_default_iface() {
                        Some(v.clone())
                    } else {
                        None
                    }
                })
                .expect("No default interface");

            let handle = iface.sockets().lock_irqsave().add(socket);
            return Ok(Self { handle, iface });
        } else {
            let iface = get_iface_to_bind(address).ok_or(ENODEV)?;
            let handle = iface.sockets().lock_irqsave().add(socket);
            return Ok(Self { handle, iface });
        }
    }

    pub fn bind_ephemeral<T>(
        socket: T,
        // socket_type: Types,
        remote: smoltcp::wire::IpAddress,
    ) -> Result<(Self, smoltcp::wire::IpAddress), SystemError>
    where
        T: smoltcp::socket::AnySocket<'static>,
    {
        let (iface, address) = get_ephemeral_iface(&remote);
        // let bound_port = iface.port_manager().bind_ephemeral_port(socket_type)?;
        let handle = iface.sockets().lock_no_preempt().add(socket);
        // let endpoint = smoltcp::wire::IpEndpoint::new(local_addr, bound_port);
        Ok((Self { handle, iface }, address))
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

    pub fn release(&self) {
        self.iface.sockets().lock().remove(self.handle);
    }
}

#[inline]
pub fn get_iface_to_bind(ip_addr: &smoltcp::wire::IpAddress) -> Option<Arc<dyn Iface>> {
    // log::debug!("get_iface_to_bind: {:?}", ip_addr);
    // if ip_addr.is_unspecified()
    crate::net::NET_DEVICES
        .read_irqsave()
        .iter()
        .find(|(_, iface)| {
            let guard = iface.smol_iface().lock();
            // log::debug!("iface name: {}, ip: {:?}", iface.iface_name(), guard.ip_addrs());
            return guard.has_ip_addr(*ip_addr);
        })
        .map(|(_, iface)| iface.clone())
}

/// Get a suitable iface to deal with sendto/connect request if the socket is not bound to an iface.
/// If the remote address is the same as that of some iface, we will use the iface.
/// Otherwise, we will use a default interface.
fn get_ephemeral_iface(
    remote_ip_addr: &smoltcp::wire::IpAddress,
) -> (Arc<dyn Iface>, smoltcp::wire::IpAddress) {
    get_iface_to_bind(remote_ip_addr)
        .map(|iface| (iface, *remote_ip_addr))
        .or({
            let ifaces = NET_DEVICES.read_irqsave();
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
            NET_DEVICES.read_irqsave().values().next().map(|iface| {
                (
                    iface.clone(),
                    iface.smol_iface().lock().ip_addrs()[0].address(),
                )
            })
        })
        .expect("No network interface")
}
