use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::driver::net::Iface;

use super::uapi::{packet_mreq_type, PacketMreq};

#[derive(Debug)]
struct PacketMreqEntry {
    mreq: PacketMreq,
    count: usize,
}

#[derive(Debug, Default)]
pub(super) struct PacketMembershipState {
    closed: bool,
    entries: Vec<PacketMreqEntry>,
}

fn mreq_match(a: &PacketMreq, b: &PacketMreq) -> bool {
    a.mr_ifindex == b.mr_ifindex
        && a.mr_type == b.mr_type
        && a.mr_alen == b.mr_alen
        && a.mr_address[..a.mr_alen as usize] == b.mr_address[..a.mr_alen as usize]
}

impl super::PacketSocket {
    pub(super) fn add_membership(&self, value: &[u8]) -> Result<(), SystemError> {
        let mreq = parse_mreq(value)?;
        let iface = self.find_iface(mreq.mr_ifindex as u32)?;
        validate_add_mreq(&mreq, iface.mac().as_bytes().len())?;

        let mut state = self.memberships.lock();
        if state.closed {
            return Err(SystemError::EBADF);
        }
        if let Some(entry) = state
            .entries
            .iter_mut()
            .find(|entry| mreq_match(&entry.mreq, &mreq))
        {
            entry.count = entry.count.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
            return Ok(());
        }

        state
            .entries
            .try_reserve(1)
            .map_err(|_| SystemError::ENOBUFS)?;
        apply_membership(&iface, &mreq, 1)?;
        state.entries.push(PacketMreqEntry { mreq, count: 1 });
        drop(state);
        notify_membership_change(&iface, &mreq);
        Ok(())
    }

    pub(super) fn drop_membership(&self, value: &[u8]) -> Result<(), SystemError> {
        let mreq = parse_mreq(value)?;

        let mut state = self.memberships.lock();
        let Some(pos) = state
            .entries
            .iter()
            .position(|entry| mreq_match(&entry.mreq, &mreq))
        else {
            return Ok(());
        };

        if state.entries[pos].count > 1 {
            state.entries[pos].count -= 1;
            return Ok(());
        }

        let entry = state.entries.remove(pos);
        drop(state);

        if let Ok(iface) = self.find_iface(entry.mreq.mr_ifindex as u32) {
            if apply_membership(&iface, &entry.mreq, -1).is_ok() {
                notify_membership_change(&iface, &entry.mreq);
            }
        }
        Ok(())
    }

    pub(crate) fn revert_all_memberships(&self) {
        let entries = {
            let mut state = self.memberships.lock();
            state.closed = true;
            core::mem::take(&mut state.entries)
        };

        for entry in entries {
            if let Ok(iface) = self.find_iface(entry.mreq.mr_ifindex as u32) {
                if apply_membership(&iface, &entry.mreq, -1).is_ok() {
                    notify_membership_change(&iface, &entry.mreq);
                }
            }
        }
    }
}

fn parse_mreq(value: &[u8]) -> Result<PacketMreq, SystemError> {
    if value.len() < core::mem::size_of::<PacketMreq>() {
        return Err(SystemError::EINVAL);
    }
    let mr_alen = u16::from_ne_bytes(value[6..8].try_into().unwrap());
    let required = 8usize
        .checked_add(mr_alen as usize)
        .ok_or(SystemError::EINVAL)?;
    if value.len() < required || mr_alen as usize > 8 {
        return Err(SystemError::EINVAL);
    }

    Ok(PacketMreq {
        mr_ifindex: i32::from_ne_bytes(value[0..4].try_into().unwrap()),
        mr_type: u16::from_ne_bytes(value[4..6].try_into().unwrap()),
        mr_alen,
        mr_address: value[8..16].try_into().unwrap(),
    })
}

fn validate_add_mreq(mreq: &PacketMreq, address_len: usize) -> Result<(), SystemError> {
    let mreq_len = mreq.mr_alen as usize;
    if mreq_len > address_len
        || (matches!(
            mreq.mr_type,
            packet_mreq_type::PACKET_MR_MULTICAST | packet_mreq_type::PACKET_MR_UNICAST
        ) && mreq_len != address_len)
    {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn apply_membership(
    iface: &Arc<dyn Iface>,
    mreq: &PacketMreq,
    inc: i32,
) -> Result<(), SystemError> {
    let common = iface.common();
    match mreq.mr_type {
        packet_mreq_type::PACKET_MR_PROMISC => common.adjust_promiscuity(inc),
        packet_mreq_type::PACKET_MR_ALLMULTI => common.adjust_allmulti(inc),
        _ => Ok(()),
    }
}

fn notify_membership_change(iface: &Arc<dyn Iface>, mreq: &PacketMreq) {
    if matches!(
        mreq.mr_type,
        packet_mreq_type::PACKET_MR_PROMISC | packet_mreq_type::PACKET_MR_ALLMULTI
    ) {
        crate::net::socket::netlink::notify_link_change(iface);
    }
}
