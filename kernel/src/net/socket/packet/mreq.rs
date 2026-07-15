use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::libs::mutex::Mutex;

use super::uapi::{packet_mreq_type, PacketMreq};

/// AF_PACKET 多播成员项，带引用计数。
#[derive(Debug)]
pub(super) struct PacketMreqEntry {
    pub mreq: PacketMreq,
    pub count: usize,
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
        validate_mreq(&mreq)?;

        let mut list = self.mreq_list.lock();
        if let Some(entry) = list.iter_mut().find(|e| mreq_match(&e.mreq, &mreq)) {
            entry.count += 1;
            return Ok(());
        }
        apply_membership(&iface, &mreq, 1);
        list.push(PacketMreqEntry { mreq, count: 1 });
        Ok(())
    }

    pub(super) fn drop_membership(&self, value: &[u8]) -> Result<(), SystemError> {
        let mreq = parse_mreq(value)?;

        let mut list = self.mreq_list.lock();
        let pos = list
            .iter()
            .position(|e| mreq_match(&e.mreq, &mreq))
            .ok_or(SystemError::EADDRNOTAVAIL)?;

        if list[pos].count > 1 {
            list[pos].count -= 1;
            return Ok(());
        }

        let entry = list.remove(pos);
        drop(list);

        // 设备可能已消失，best-effort revert
        if let Ok(iface) = self.find_iface(entry.mreq.mr_ifindex as u32) {
            apply_membership(&iface, &entry.mreq, -1);
        }
        Ok(())
    }

    /// socket 关闭时 revert 所有成员关系，每个 entry revert -1。
    pub(crate) fn revert_all_memberships(&self) {
        let mut list = self.mreq_list.lock();
        let entries = list.drain(..).collect::<Vec<_>>();
        drop(list);

        for entry in entries {
            if let Ok(iface) = self.find_iface(entry.mreq.mr_ifindex as u32) {
                apply_membership(&iface, &entry.mreq, -1);
            }
        }
    }
}

fn parse_mreq(value: &[u8]) -> Result<PacketMreq, SystemError> {
    if value.len() < core::mem::size_of::<PacketMreq>() {
        return Err(SystemError::EINVAL);
    }
    Ok(PacketMreq {
        mr_ifindex: i32::from_ne_bytes(value[0..4].try_into().unwrap()),
        mr_type: u16::from_ne_bytes(value[4..6].try_into().unwrap()),
        mr_alen: u16::from_ne_bytes(value[6..8].try_into().unwrap()),
        mr_address: value[8..16].try_into().unwrap(),
    })
}

fn validate_mreq(mreq: &PacketMreq) -> Result<(), SystemError> {
    if mreq.mr_alen as usize > mreq.mr_address.len()
        || mreq.mr_type > packet_mreq_type::PACKET_MR_UNICAST
    {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn apply_membership(iface: &Arc<dyn Iface>, mreq: &PacketMreq, inc: i32) {
    let common = iface.common();
    match mreq.mr_type {
        packet_mreq_type::PACKET_MR_PROMISC => common.adjust_promiscuity(inc),
        packet_mreq_type::PACKET_MR_ALLMULTI => common.adjust_allmulti(inc),
        _ => {} // MULTICAST/UNICAST：当前无硬件过滤支持
    }
}
