//! TCP protocol handles belong to a namespace, not an address-owning device.

use alloc::{boxed::Box, sync::Arc};
use smoltcp::{iface::SocketHandle, socket::tcp, wire::IpAddress};
use system_error::SystemError;

use crate::net::{socket::inet::common, tcp_stack::TcpStack, Iface};
use crate::process::namespace::net_namespace::NetNamespace;

#[derive(Debug)]
pub(crate) struct TcpBound {
    handle: SocketHandle,
    netns: Arc<NetNamespace>,
}

impl Drop for TcpBound {
    fn drop(&mut self) {
        // The last in-flight TcpSocket user may have kept close-defer from
        // reclaiming its handle. Do not access that handle here: release() or
        // into_socket() may already have removed it.
        self.netns.wakeup_tcp_poll();
    }
}

impl TcpBound {
    /// Used for already validated endpoints and replacement listener slots.
    pub fn new(socket: tcp::Socket<'static>, netns: Arc<NetNamespace>) -> Self {
        let handle = netns.tcp_stack().sockets().lock().add(socket);
        Self { handle, netns }
    }

    pub fn bind_recoverable(
        socket: Box<tcp::Socket<'static>>,
        address: &IpAddress,
        netns: Arc<NetNamespace>,
    ) -> Result<Self, (Box<tcp::Socket<'static>>, SystemError)> {
        if !address.is_unspecified() && common::get_iface_for_local_bind(address, &netns).is_none()
        {
            let error = common::bind_addr_not_found_error(address, &netns);
            return Err((socket, error));
        }
        Ok(Self::new(*socket, netns))
    }

    pub fn bind_ephemeral_recoverable_on_device(
        socket: Box<tcp::Socket<'static>>,
        remote: IpAddress,
        netns: Arc<NetNamespace>,
        device: Option<Arc<dyn Iface>>,
    ) -> Result<(Self, IpAddress), (Box<tcp::Socket<'static>>, SystemError)> {
        let target = match common::tcp_connect_target(&remote, &netns, device) {
            Ok(target) => target,
            Err(error) => return Err((socket, error)),
        };
        Ok((Self::new(*socket, netns), target.local_addr))
    }

    pub fn with_mut<T: smoltcp::socket::AnySocket<'static>, R, F: FnMut(&mut T) -> R>(
        &self,
        mut f: F,
    ) -> R {
        f(self.stack().sockets().lock().get_mut::<T>(self.handle))
    }

    pub fn with<T: smoltcp::socket::AnySocket<'static>, R, F: Fn(&T) -> R>(&self, f: F) -> R {
        f(self.stack().sockets().lock().get::<T>(self.handle))
    }

    pub fn stack(&self) -> &Arc<TcpStack> {
        self.netns.tcp_stack()
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    pub fn handle(&self) -> SocketHandle {
        self.handle
    }

    pub fn release(&self) {
        self.stack().sockets().lock().remove(self.handle);
    }

    pub fn into_socket(self) -> smoltcp::socket::Socket<'static> {
        self.stack().sockets().lock().remove(self.handle)
    }
}
