//! Multicast loopback support for UDP sockets
//!
//! This module implements IP_MULTICAST_LOOP functionality by maintaining a registry
//! of sockets that have joined multicast groups and delivering looped-back packets
//! directly to their receive buffers.

use alloc::sync::Weak;
use alloc::vec::Vec;

use crate::libs::rwsem::RwSem;

use super::UdpSocket;

/// A registered multicast group member
#[derive(Clone)]
struct MulticastMember {
    /// Weak reference to the socket
    socket: Weak<UdpSocket>,
    /// Multicast group address (in network byte order for IPv4)
    multiaddr: u32,
    /// Interface index where the group was joined
    ifindex: i32,
}

/// Global registry of multicast group memberships
pub struct MulticastLoopbackRegistry {
    members: RwSem<Vec<MulticastMember>>,
}

impl MulticastLoopbackRegistry {
    pub const fn new() -> Self {
        Self {
            members: RwSem::new(Vec::new()),
        }
    }

    /// Register a socket as a member of a multicast group
    pub fn register(&self, socket: Weak<UdpSocket>, multiaddr: u32, ifindex: i32) {
        let mut members = self.members.write();
        // Check if already registered
        let exists = members.iter().any(|m| {
            m.multiaddr == multiaddr && m.ifindex == ifindex && m.socket.as_ptr() == socket.as_ptr()
        });
        if !exists {
            members.push(MulticastMember {
                socket,
                multiaddr,
                ifindex,
            });
        }
    }

    /// Unregister a socket from a multicast group
    pub fn unregister(&self, socket: &Weak<UdpSocket>, multiaddr: u32, ifindex: i32) {
        let mut members = self.members.write();
        members.retain(|m| {
            !(m.multiaddr == multiaddr
                && m.ifindex == ifindex
                && m.socket.as_ptr() == socket.as_ptr())
        });
    }

    /// Unregister a socket from all multicast groups (called on socket close)
    pub fn unregister_all(&self, socket: &Weak<UdpSocket>) {
        let mut members = self.members.write();
        members.retain(|m| m.socket.as_ptr() != socket.as_ptr());
    }

    pub fn has_membership(&self, multiaddr: u32, ifindex: i32) -> bool {
        let members = self.members.read();
        members
            .iter()
            .any(|m| m.multiaddr == multiaddr && m.ifindex == ifindex)
    }
}

/// Global multicast loopback registry
static MULTICAST_REGISTRY: MulticastLoopbackRegistry = MulticastLoopbackRegistry::new();

/// Get the global multicast loopback registry
pub fn multicast_registry() -> &'static MulticastLoopbackRegistry {
    &MULTICAST_REGISTRY
}
