//! Namespace-owned service-port authorization and ephemeral-range invariant.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use smoltcp::wire::{IpEndpoint, IpVersion};
use system_error::SystemError;

use crate::process::{
    cred::{ns_capable, CAPFlags},
    namespace::net_namespace::NetNamespace,
};

/// One snapshot prevents independently valid sysctl writes from producing an
/// ephemeral range that includes privileged ports. Bind only needs one load.
#[derive(Debug)]
pub struct InetPortPolicy(AtomicU64);

impl InetPortPolicy {
    pub const fn new() -> Self {
        Self(AtomicU64::new(
            ((super::port::DEFAULT_LOCAL_PORT_RANGE as u64) << 16) | 1024,
        ))
    }

    pub fn local_range(&self) -> (u16, u16) {
        let state = self.0.load(Ordering::Relaxed);
        ((state >> 32) as u16, (state >> 16) as u16)
    }

    pub fn unprivileged_start(&self) -> u16 {
        self.0.load(Ordering::Relaxed) as u16
    }

    pub fn set_local_range(&self, min: u16, max: u16) -> Result<(), SystemError> {
        if min == 0 || min > max {
            return Err(SystemError::EINVAL);
        }
        self.update(|old| {
            if min < old as u16 {
                return Err(SystemError::EINVAL);
            }
            Ok(((min as u64) << 32) | ((max as u64) << 16) | (old & 0xffff))
        })
    }

    pub fn set_unprivileged_start(&self, start: u16) -> Result<(), SystemError> {
        self.update(|old| {
            if start > (old >> 32) as u16 {
                return Err(SystemError::EINVAL);
            }
            Ok((old & !0xffff) | start as u64)
        })
    }

    fn update(&self, make: impl Fn(u64) -> Result<u64, SystemError>) -> Result<(), SystemError> {
        let mut old = self.0.load(Ordering::Relaxed);
        loop {
            let new = make(old)?;
            match self
                .0
                .compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(current) => old = current,
            }
        }
    }
}

/// Called only for explicit TCP/UDP bind, before state changes or reservation.
/// IPv4 checks local addresses first; IPv6 checks capability first, including
/// mapped addresses. The original socket family controls this ordering.
pub(crate) fn check_bind_access(
    netns: &Arc<NetNamespace>,
    endpoint: IpEndpoint,
    version: IpVersion,
) -> Result<(), SystemError> {
    if version == IpVersion::Ipv4
        && !endpoint.addr.is_unspecified()
        && super::get_iface_for_local_bind(&endpoint.addr, netns).is_none()
    {
        return Err(super::bind_addr_not_found_error(&endpoint.addr, netns));
    }
    if endpoint.port != 0
        && endpoint.port < netns.unprivileged_port_start()
        && !ns_capable(netns.user_ns(), CAPFlags::CAP_NET_BIND_SERVICE)
    {
        return Err(SystemError::EACCES);
    }
    Ok(())
}
