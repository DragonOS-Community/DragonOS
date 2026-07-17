use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::filesystem::epoll::EPollEventType;
use crate::net::socket::endpoint::{Endpoint, LinkLayerEndpoint};

use super::{PacketIngressMetadata, PacketSocket};

/// Lock-free, coherent `(ifindex, protocol)` receive-filter snapshot.
#[derive(Debug)]
pub(crate) struct PacketBinding(AtomicU64);

impl PacketBinding {
    pub(crate) fn new(ifindex: u32, protocol: u16) -> Self {
        Self(AtomicU64::new(Self::pack(ifindex, protocol)))
    }
    const fn pack(ifindex: u32, protocol: u16) -> u64 {
        ((ifindex as u64) << 32) | protocol as u64
    }
    pub(crate) fn load(&self) -> (u32, u16) {
        let value = self.0.load(Ordering::Acquire);
        ((value >> 32) as u32, value as u16)
    }
    pub(crate) fn store(&self, ifindex: u32, protocol: u16) {
        self.0
            .store(Self::pack(ifindex, protocol), Ordering::Release);
    }
}

impl PacketSocket {
    pub(super) fn find_iface(&self, ifindex: u32) -> Result<Arc<dyn Iface>, SystemError> {
        self.netns
            .device_list()
            .values()
            .find(|x| x.nic_id() == ifindex as usize)
            .cloned()
            .ok_or(SystemError::ENODEV)
    }

    pub fn bind_to_interface(&self, ifindex: i32) -> Result<(), SystemError> {
        let protocol = self.binding.load().1;
        self.bind_to(ifindex, protocol)
    }

    fn bind_to(&self, ifindex: i32, requested_protocol: u16) -> Result<(), SystemError> {
        if ifindex < 0 {
            return Err(SystemError::ENODEV);
        }
        let _guard = self.bind_lock.lock();
        if self.has_fanout_group() {
            return Err(SystemError::EINVAL);
        }
        let (old_index, old_protocol) = self.binding.load();
        let protocol = if requested_protocol == 0 {
            old_protocol
        } else {
            requested_protocol
        };
        let new_iface = if ifindex == 0 {
            None
        } else {
            Some(self.find_iface(ifindex as u32)?)
        };
        if old_index == ifindex as u32 && old_protocol == protocol {
            return Ok(());
        }

        *self.bound_iface.write() = new_iface;
        self.binding.store(ifindex as u32, protocol);
        Ok(())
    }

    pub(super) fn bind_endpoint(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        match endpoint {
            Endpoint::LinkLayer(ll) => self.bind_to(ll.interface as i32, ll.protocol),
            _ => Err(SystemError::EAFNOSUPPORT),
        }
    }

    pub(super) fn close_binding(&self) -> Result<(), SystemError> {
        let _guard = self.bind_lock.lock();
        self.revert_all_memberships();
        self.netns.unregister_packet_socket(&self.self_ref);
        *self.bound_iface.write() = None;
        self.binding.store(0, 0);
        Ok(())
    }

    pub(super) fn packet_local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let _guard = self.bind_lock.lock();
        let (ifindex, protocol) = self.binding.load();
        let mut ll = LinkLayerEndpoint::new(ifindex as usize);
        ll.protocol = protocol;
        if let Some(iface) = self.bound_iface.read().as_ref() {
            ll.addr[..6].copy_from_slice(iface.mac().as_bytes());
            ll.hatype = iface.type_() as u16;
            ll.halen = 6;
        }
        Ok(Endpoint::LinkLayer(ll))
    }

    pub(super) fn packet_io_event(&self) -> EPollEventType {
        use crate::filesystem::epoll::EPollEventType as E;
        let mut out = E::empty();
        if self.can_recv() {
            out.insert(E::EPOLLIN | E::EPOLLRDNORM);
        }
        out.insert(E::EPOLLOUT | E::EPOLLWRNORM | E::EPOLLWRBAND);
        out
    }

    /// Registry delivery entry: the ingress interface is authoritative metadata.
    pub(crate) fn deliver(&self, ingress: PacketIngressMetadata, frame: &[u8]) {
        self.deliver_from_iface(ingress, frame);
    }
}
