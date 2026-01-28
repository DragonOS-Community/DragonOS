use alloc::sync::Arc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use smoltcp::wire::{IpAddress, Ipv4Address};
use system_error::SystemError;

use crate::libs::mutex::Mutex;
use crate::net::socket::IpOption;
use crate::net::Iface;
use crate::process::namespace::net_namespace::NetNamespace;

pub const IP_MREQ_SIZE: usize = 8;
pub const IP_MREQN_SIZE: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4MulticastMembership {
    pub multiaddr: u32,
    pub ifindex: i32,
    pub ifaddr: u32,
}

#[inline]
pub fn is_ipv4_multicast(addr_be: u32) -> bool {
    let b = addr_be.to_ne_bytes();
    (224..=239).contains(&b[0])
}

pub fn find_iface_by_ifindex(netns: &Arc<NetNamespace>, ifindex: i32) -> Option<Arc<dyn Iface>> {
    if ifindex <= 0 {
        return None;
    }
    let ifindex = ifindex as usize;
    if let Some(iface) = netns.device_list().get(&ifindex) {
        return Some(iface.clone());
    }
    netns
        .loopback_iface()
        .and_then(|lo| {
            if lo.nic_id() == ifindex {
                Some(lo)
            } else {
                None
            }
        })
        .map(|lo| lo as Arc<dyn Iface>)
}

pub fn find_iface_by_ipv4(netns: &Arc<NetNamespace>, addr_be: u32) -> Option<Arc<dyn Iface>> {
    let b = addr_be.to_ne_bytes();
    let target = Ipv4Address::new(b[0], b[1], b[2], b[3]);

    for (_id, iface) in netns.device_list().iter() {
        let smol_iface = iface.smol_iface().lock();
        if smol_iface
            .ip_addrs()
            .iter()
            .any(|cidr| match cidr.address() {
                IpAddress::Ipv4(v4) => v4 == target,
                _ => false,
            })
        {
            return Some(iface.clone());
        }
    }

    netns.loopback_iface().and_then(|lo| {
        let found = {
            let smol_iface = lo.smol_iface().lock();
            smol_iface
                .ip_addrs()
                .iter()
                .any(|cidr| match cidr.address() {
                    IpAddress::Ipv4(v4) => v4 == target,
                    _ => false,
                })
        };
        if found {
            Some(lo as Arc<dyn Iface>)
        } else {
            None
        }
    })
}

pub fn choose_default_ipv4_iface(netns: &Arc<NetNamespace>) -> Option<Arc<dyn Iface>> {
    if let Some(iface) = netns.default_iface() {
        return Some(iface);
    }
    if let Some(iface) = netns.device_list().values().next() {
        return Some(iface.clone());
    }
    netns.loopback_iface().map(|lo| lo as Arc<dyn Iface>)
}

pub fn parse_mreqn_for_multicast_if(val: &[u8]) -> Result<(u32, i32), SystemError> {
    if val.len() < core::mem::size_of::<u32>() {
        return Err(SystemError::EINVAL);
    }
    // Default: in_addr only.
    let mut ifaddr = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
    let mut ifindex = 0i32;

    if val.len() >= IP_MREQN_SIZE {
        ifaddr = u32::from_ne_bytes([val[4], val[5], val[6], val[7]]);
        ifindex = i32::from_ne_bytes([val[8], val[9], val[10], val[11]]);
    } else if val.len() >= IP_MREQ_SIZE {
        ifaddr = u32::from_ne_bytes([val[4], val[5], val[6], val[7]]);
    }

    Ok((ifaddr, ifindex))
}

pub fn parse_mreqn_for_membership(val: &[u8]) -> Result<(u32, u32, i32), SystemError> {
    if val.len() < IP_MREQ_SIZE {
        return Err(SystemError::EINVAL);
    }
    let multiaddr = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
    let ifaddr = u32::from_ne_bytes([val[4], val[5], val[6], val[7]]);
    let mut ifindex = 0i32;
    if val.len() >= IP_MREQN_SIZE {
        ifindex = i32::from_ne_bytes([val[8], val[9], val[10], val[11]]);
    }
    Ok((multiaddr, ifaddr, ifindex))
}

pub fn apply_ipv4_multicast_if(
    netns: &Arc<NetNamespace>,
    val: &[u8],
    ifindex: &AtomicI32,
    ifaddr: &AtomicU32,
) -> Result<(), SystemError> {
    let (addr, index) = parse_mreqn_for_multicast_if(val)?;
    if index < 0 {
        return Err(SystemError::EADDRNOTAVAIL);
    }
    if index == 0 && addr == 0 {
        ifindex.store(0, Ordering::Relaxed);
        ifaddr.store(0, Ordering::Relaxed);
        return Ok(());
    }
    let iface = if index != 0 {
        find_iface_by_ifindex(netns, index)
    } else {
        find_iface_by_ipv4(netns, addr)
    }
    .ok_or(SystemError::EADDRNOTAVAIL)?;
    ifindex.store(iface.nic_id() as i32, Ordering::Relaxed);
    ifaddr.store(addr, Ordering::Relaxed);
    Ok(())
}

pub fn apply_ipv4_membership(
    netns: &Arc<NetNamespace>,
    opt: IpOption,
    val: &[u8],
    groups: &Mutex<Vec<Ipv4MulticastMembership>>,
) -> Result<(), SystemError> {
    let (multi, ifaddr, ifindex) = parse_mreqn_for_membership(val)?;
    if !is_ipv4_multicast(multi) {
        return Err(SystemError::EINVAL);
    }
    if ifindex < 0 {
        return Err(SystemError::ENODEV);
    }
    let iface = if ifindex != 0 {
        find_iface_by_ifindex(netns, ifindex)
    } else if ifaddr != 0 {
        find_iface_by_ipv4(netns, ifaddr)
    } else {
        choose_default_ipv4_iface(netns)
    };

    if iface.is_none() {
        if opt == IpOption::DROP_MEMBERSHIP && ifindex == 0 && ifaddr == 0 {
            return Err(SystemError::ENODEV);
        }
        return Err(if opt == IpOption::DROP_MEMBERSHIP {
            SystemError::EADDRNOTAVAIL
        } else {
            SystemError::ENODEV
        });
    }

    let resolved_ifindex = iface.unwrap().nic_id() as i32;
    let mut groups = groups.lock();
    match opt {
        IpOption::ADD_MEMBERSHIP => {
            if groups
                .iter()
                .any(|g| g.multiaddr == multi && g.ifindex == resolved_ifindex)
            {
                return Err(SystemError::EADDRINUSE);
            }
            groups.push(Ipv4MulticastMembership {
                multiaddr: multi,
                ifindex: resolved_ifindex,
                ifaddr,
            });
            Ok(())
        }
        IpOption::DROP_MEMBERSHIP => {
            let pos = groups.iter().position(|g| {
                if g.multiaddr != multi {
                    return false;
                }
                if ifindex != 0 {
                    return g.ifindex == resolved_ifindex;
                }
                if ifaddr != 0 {
                    return g.ifaddr == ifaddr;
                }
                true
            });
            if let Some(idx) = pos {
                groups.swap_remove(idx);
                Ok(())
            } else {
                Err(SystemError::EADDRNOTAVAIL)
            }
        }
        _ => Err(SystemError::ENOPROTOOPT),
    }
}
