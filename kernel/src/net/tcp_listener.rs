//! Logical TCP listeners outlive the individual smoltcp accept slots.
//!
//! Consulted after TCP validation and ordinary socket demultiplexing: an
//! unmatched new SYN for a live listener means capacity pressure, not a closed
//! port. No transport-state prediction belongs in this registry.

use alloc::vec::Vec;
use smoltcp::wire::IpEndpoint;

use crate::libs::rwsem::RwSem;
use crate::net::socket::inet::common::port::TcpBindDomain;

#[derive(Debug)]
struct Listener {
    id: u64,
    domain: TcpBindDomain,
    port: u16,
    device: u32,
}

/// Namespace TCP listener facts, with no owning socket or interface references.
#[derive(Debug)]
pub struct TcpListenerRegistry {
    listeners: RwSem<Vec<Listener>>,
}

impl TcpListenerRegistry {
    pub fn new() -> Self {
        Self {
            listeners: RwSem::new(Vec::new()),
        }
    }

    pub fn register(&self, id: u64, domain: TcpBindDomain, port: u16, device: u32) {
        let mut listeners = self.listeners.write();
        let entry = Listener {
            id,
            domain,
            port,
            device,
        };
        if let Some(existing) = listeners.iter_mut().find(|listener| listener.id == id) {
            *existing = entry;
        } else {
            listeners.push(entry);
        }
    }

    pub fn unregister(&self, id: u64) {
        let mut listeners = self.listeners.write();
        if let Some(index) = listeners.iter().position(|listener| listener.id == id) {
            listeners.swap_remove(index);
        }
    }
}

impl smoltcp::iface::TcpListenRegistry for TcpListenerRegistry {
    fn is_listening(&self, local_endpoint: IpEndpoint, meta: smoltcp::phy::PacketMeta) -> bool {
        // Called with SocketSet locked: never acquire socket/iface locks here.
        self.listeners.read().iter().any(|listener| {
            listener.port == local_endpoint.port
                && listener.domain.matches(local_endpoint.addr)
                && (listener.device == 0 || listener.device == meta.id)
        })
    }
}
