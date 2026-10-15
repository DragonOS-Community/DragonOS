//! IPv6 UDP ASM ownership. Interface admission/MLD belongs to smoltcp;
//! this list implements Linux inet6_mc_check's socket-level group predicate.

use alloc::sync::{Arc, Weak};
use smoltcp::wire::{IpAddress, Ipv6Address};
use system_error::SystemError;

use super::UdpSocket;
use crate::{net::socket::PIPV6, net::Iface};

pub(super) struct Membership {
    group: Ipv6Address,
    ifindex: u32,
    iface: Weak<dyn Iface>,
    epoch: u64,
}

impl core::fmt::Debug for Membership {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Membership")
            .field("group", &self.group)
            .field("ifindex", &self.ifindex)
            .field("epoch", &self.epoch)
            .finish()
    }
}

fn request(opt: PIPV6, value: &[u8]) -> Result<(Ipv6Address, u32), SystemError> {
    let (offset, ifindex) = match opt {
        PIPV6::ADD_MEMBERSHIP | PIPV6::DROP_MEMBERSHIP => {
            if value.len() < 20 {
                return Err(SystemError::EINVAL);
            }
            (0, u32::from_ne_bytes(value[16..20].try_into().unwrap()))
        }
        PIPV6::MCAST_JOIN_GROUP | PIPV6::MCAST_LEAVE_GROUP => {
            // Native group_req: u32 interface, padding, sockaddr_storage.
            if value.len() < 136 {
                return Err(SystemError::EINVAL);
            }
            if u16::from_ne_bytes(value[8..10].try_into().unwrap()) != 10 {
                return Err(SystemError::EADDRNOTAVAIL);
            }
            (16, u32::from_ne_bytes(value[..4].try_into().unwrap()))
        }
        _ => return Err(SystemError::ENOPROTOOPT),
    };
    let group = Ipv6Address::from(<[u8; 16]>::try_from(&value[offset..offset + 16]).unwrap());
    if !group.is_multicast() {
        return Err(SystemError::EINVAL);
    }
    Ok((group, ifindex))
}

impl UdpSocket {
    pub(super) fn set_ipv6_membership(&self, opt: PIPV6, value: &[u8]) -> Result<(), SystemError> {
        let (group, requested) = request(opt, value)?;
        // Match close's existing writer serialization. Never hold the member
        // list while taking smoltcp: RX holds smoltcp before reading this list.
        let _placement = self.iface_placement.write();
        if self.inner.read().is_none() {
            return Err(SystemError::EBADF);
        }
        let join = matches!(opt, PIPV6::ADD_MEMBERSHIP | PIPV6::MCAST_JOIN_GROUP);
        if !join {
            let entry = {
                let mut groups = self.ipv6_multicast_groups.lock();
                let position = groups
                    .iter()
                    .rposition(|entry| {
                        entry.group == group && (requested == 0 || entry.ifindex == requested)
                    })
                    .ok_or(SystemError::EADDRNOTAVAIL)?;
                groups.remove(position)
            };
            self.release_ipv6_membership(entry);
            return Ok(());
        }
        {
            let mut groups = self.ipv6_multicast_groups.lock();
            if groups
                .iter()
                .any(|entry| entry.group == group && (requested == 0 || entry.ifindex == requested))
            {
                return Err(SystemError::EADDRINUSE);
            }
            groups.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        }
        // Membership chooses only a FIB device. Unlike send, it must not
        // require a configured unicast source or consult MULTICAST_IF.
        let ifindex = if requested == 0 {
            let router = self.netns.router();
            let routes = crate::net::route::lock_output_routes(&router, self.netns.device_list());
            routes
                .lookup(IpAddress::Ipv6(group), None)
                .ok_or(SystemError::ENODEV)?
                .oif
        } else {
            requested
        };
        let iface = crate::net::socket::inet::common::multicast::find_iface_by_ifindex(
            &self.netns,
            ifindex as i32,
        )
        .ok_or(SystemError::ENODEV)?;
        let epoch = iface.common().namespace_epoch();
        let _publication = iface
            .common()
            .enter_socket_publication(epoch)
            .ok_or(SystemError::ENODEV)?;
        iface
            .common()
            .multicast_join_ref(&self.netns, group.into())?;
        // No allocation or fallible step follows the device join.
        self.ipv6_multicast_groups.lock().push(Membership {
            group,
            ifindex,
            iface: Arc::downgrade(&iface),
            epoch,
        });
        Ok(())
    }

    fn release_ipv6_membership(&self, entry: Membership) {
        if let Some(iface) = entry.iface.upgrade() {
            if let Some(_publication) = iface.common().enter_socket_publication(entry.epoch) {
                let _ = iface
                    .common()
                    .multicast_leave_ref(&self.netns, entry.group.into());
            }
        }
    }

    /// Called while close owns iface_placement's write side.
    pub(super) fn drop_ipv6_memberships(&self) {
        let groups = core::mem::take(&mut *self.ipv6_multicast_groups.lock());
        for entry in groups {
            self.release_ipv6_membership(entry);
        }
    }

    pub(super) fn accepts_multicast(&self, destination: IpAddress, ifindex: i32) -> bool {
        match destination {
            IpAddress::Ipv4(group) => {
                self.ip_multicast_all
                    .load(core::sync::atomic::Ordering::Relaxed)
                    || self
                        .has_ipv4_multicast_membership(u32::from_ne_bytes(group.octets()), ifindex)
            }
            // Linux inet6_mc_check is intentionally group-only. The ingress
            // device has already passed smoltcp's actual membership check.
            IpAddress::Ipv6(group) => {
                self.ipv6_multicast_all
                    .load(core::sync::atomic::Ordering::Relaxed)
                    || self
                        .ipv6_multicast_groups
                        .lock()
                        .iter()
                        .any(|entry| entry.group == group)
            }
        }
    }
}
