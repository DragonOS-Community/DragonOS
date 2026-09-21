use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use hashbrown::HashMap;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpVersion};
use system_error::SystemError;

use crate::process::namespace::net_namespace::NetNamespace;
use crate::{arch::rand::rand, libs::mutex::Mutex, process::ProcessManager};

/// A normalized TCP receive domain. Mapped IPv6 addresses are normalized to
/// IPv4 before construction; only the IPv6 wildcard may cover both families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpBindDomain {
    pub addr: IpAddress,
    pub ip_version: Option<IpVersion>,
}

impl TcpBindDomain {
    pub fn new(addr: IpAddress, v6_only: bool) -> Self {
        let ip_version = if addr.version() == IpVersion::Ipv6 && addr.is_unspecified() && !v6_only {
            None
        } else {
            Some(addr.version())
        };
        Self { addr, ip_version }
    }

    pub fn matches(&self, addr: IpAddress) -> bool {
        self.ip_version
            .is_none_or(|version| version == addr.version())
            && (self.addr.is_unspecified() || self.addr == addr)
    }

    fn overlaps(&self, other: Self) -> bool {
        if let (Some(a), Some(b)) = (self.ip_version, other.ip_version) {
            if a != b {
                return false;
            }
        }
        self.addr.is_unspecified() || other.addr.is_unspecified() || self.addr == other.addr
    }

    pub fn listen_endpoint(&self, port: u16) -> IpListenEndpoint {
        if self.addr.is_unspecified() {
            port.into()
        } else {
            IpEndpoint::new(self.addr, port).into()
        }
    }
}

#[derive(Debug)]
struct Binding {
    id: u64,
    domain: TcpBindDomain,
}

/// Network-namespace TCP reservations, independently of the interface hosting
/// the smoltcp socket. Check and insertion are atomic under one table lock.
#[derive(Debug)]
pub struct PortManager {
    bindings: Mutex<HashMap<u16, Vec<Binding>>>,
    next_id: AtomicU64,
    next_ephemeral: AtomicU16,
}

impl Default for PortManager {
    fn default() -> Self {
        Self {
            bindings: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_ephemeral: AtomicU16::new(0),
        }
    }
}

pub const DEFAULT_LOCAL_PORT_RANGE: u32 = (32768u32 << 16) | 60999u32;

/// Unique ownership of a reservation. Accepted children deliberately have no
/// reservation; state transitions move this token instead of copying a port.
#[derive(Debug)]
pub struct TcpPortReservation {
    netns: Arc<NetNamespace>,
    pub id: u64,
    pub port: u16,
    pub domain: TcpBindDomain,
}

impl Drop for TcpPortReservation {
    fn drop(&mut self) {
        self.netns.tcp_ports().unbind(self.port, self.id);
    }
}

impl PortManager {
    pub fn local_port_range() -> (u16, u16) {
        ProcessManager::current_netns().local_port_range()
    }

    pub fn set_local_port_range(min: u16, max: u16) -> Result<(), SystemError> {
        ProcessManager::current_netns().set_local_port_range(min, max)
    }

    pub fn reserve(
        netns: Arc<NetNamespace>,
        domain: TcpBindDomain,
        port: u16,
    ) -> Result<TcpPortReservation, SystemError> {
        let manager = netns.tcp_ports();
        let (min, max) = netns.local_port_range();
        let count = u32::from(max) - u32::from(min) + 1;
        let mut bindings = manager.bindings.lock();
        let initial = manager.next_ephemeral.load(Ordering::Relaxed);
        let mut candidate = if port != 0 {
            port
        } else if initial >= min && initial <= max {
            initial
        } else {
            min + (rand() % count as usize) as u16
        };
        for _ in 0..if port == 0 { count } else { 1 } {
            let bucket = bindings.entry(candidate).or_default();
            if !bucket.iter().any(|binding| domain.overlaps(binding.domain)) {
                bucket.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                let id = manager.next_id.fetch_add(1, Ordering::Relaxed);
                bucket.push(Binding { id, domain });
                if port == 0 {
                    manager.next_ephemeral.store(
                        if candidate == max { min } else { candidate + 1 },
                        Ordering::Relaxed,
                    );
                }
                drop(bindings);
                return Ok(TcpPortReservation {
                    netns,
                    id,
                    port: candidate,
                    domain,
                });
            }
            if port != 0 {
                break;
            }
            candidate = if candidate == max { min } else { candidate + 1 };
        }
        Err(SystemError::EADDRINUSE)
    }

    fn unbind(&self, port: u16, id: u64) {
        let mut bindings = self.bindings.lock();
        if let Some(bucket) = bindings.get_mut(&port) {
            bucket.retain(|binding| binding.id != id);
            if bucket.is_empty() {
                bindings.remove(&port);
            }
        }
    }
}
