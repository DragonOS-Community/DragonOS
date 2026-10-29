use alloc::vec::Vec;
use system_error::SystemError;

use crate::driver::net::{
    types::{InterfaceFlags, InterfaceType},
    Iface,
};
use crate::filesystem::vfs::iov::IoVecs;
use crate::net::posix::SockAddr;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::PMSG;

use super::{PacketSocket, PacketSocketType, SockAddrLl};

const ETHERNET_HEADER_LEN: usize = 14;
const VLAN_HEADER_LEN: usize = 4;
const IEEE_8021Q_TPID: u16 = 0x8100;

fn extra_vlan_len_allowed(iface: &dyn Iface, protocol: u16) -> bool {
    iface.type_() == InterfaceType::ETHER && protocol == IEEE_8021Q_TPID
}

struct PreparedPacketSend {
    iface: alloc::sync::Arc<dyn Iface>,
    kind: PreparedPacketKind,
}

enum PreparedPacketKind {
    Raw,
    Dgram { dest: SockAddrLl, protocol: u16 },
}

impl PacketSocket {
    fn destination_iface(
        &self,
        dest: Option<&SockAddrLl>,
    ) -> Result<alloc::sync::Arc<dyn Iface>, SystemError> {
        if let Some(addr) = dest {
            if addr.sll_ifindex < 0 {
                return Err(SystemError::ENODEV);
            }
            if addr.sll_ifindex > 0 {
                return self.find_iface(addr.sll_ifindex as u32);
            }
        }
        self.bound_iface
            .read()
            .clone()
            .ok_or(SystemError::EDESTADDRREQ)
    }

    fn prepare_send(
        &self,
        len: usize,
        dest: Option<SockAddrLl>,
    ) -> Result<PreparedPacketSend, SystemError> {
        let iface = self.destination_iface(dest.as_ref())?;
        if !iface.user_visible_flags().contains(InterfaceFlags::UP) {
            return Err(SystemError::ENETDOWN);
        }

        let kind = match self.sock_type {
            PacketSocketType::Raw => PreparedPacketKind::Raw,
            PacketSocketType::Dgram => {
                let addr = dest.ok_or(SystemError::EDESTADDRREQ)?;
                if addr.sll_halen < 6 {
                    return Err(SystemError::EINVAL);
                }
                let protocol = if addr.sll_protocol != 0 {
                    addr.sll_protocol
                } else {
                    self.binding.load().1
                };
                if protocol == 0 {
                    return Err(SystemError::EINVAL);
                }
                PreparedPacketKind::Dgram {
                    dest: addr,
                    protocol,
                }
            }
        };
        let coarse_overhead = match &kind {
            PreparedPacketKind::Raw => ETHERNET_HEADER_LEN + VLAN_HEADER_LEN,
            PreparedPacketKind::Dgram { .. } => VLAN_HEADER_LEN,
        };
        let coarse_limit = iface
            .mtu()
            .checked_add(coarse_overhead)
            .ok_or(SystemError::EMSGSIZE)?;
        if len > coarse_limit {
            return Err(SystemError::EMSGSIZE);
        }

        Ok(PreparedPacketSend { iface, kind })
    }

    fn finish_send(&self, prepared: PreparedPacketSend, buf: &[u8]) -> Result<usize, SystemError> {
        let PreparedPacketSend { iface, kind } = prepared;
        match kind {
            PreparedPacketKind::Raw => {
                if buf.len() < ETHERNET_HEADER_LEN {
                    return Err(SystemError::EINVAL);
                }
                let protocol = u16::from_be_bytes([buf[12], buf[13]]);
                let l2_len = if extra_vlan_len_allowed(iface.as_ref(), protocol) {
                    ETHERNET_HEADER_LEN + VLAN_HEADER_LEN
                } else {
                    ETHERNET_HEADER_LEN
                };
                let max_len = iface
                    .mtu()
                    .checked_add(l2_len)
                    .ok_or(SystemError::EMSGSIZE)?;
                if buf.len() > max_len {
                    return Err(SystemError::EMSGSIZE);
                }
                let _tx = iface
                    .common()
                    .try_acquire_tx()
                    .ok_or(SystemError::ENETDOWN)?;
                iface.raw_transmit(buf)?;
                Ok(buf.len())
            }
            PreparedPacketKind::Dgram { dest, protocol } => {
                let total = 14usize
                    .checked_add(buf.len())
                    .ok_or(SystemError::EMSGSIZE)?;
                let max_payload = iface
                    .mtu()
                    .checked_add(if extra_vlan_len_allowed(iface.as_ref(), protocol) {
                        VLAN_HEADER_LEN
                    } else {
                        0
                    })
                    .ok_or(SystemError::EMSGSIZE)?;
                if buf.len() > max_payload {
                    return Err(SystemError::EMSGSIZE);
                }
                let mut frame = Vec::new();
                frame
                    .try_reserve_exact(total)
                    .map_err(|_| SystemError::ENOMEM)?;
                frame.extend_from_slice(&dest.sll_addr[..6]);
                frame.extend_from_slice(iface.mac().as_bytes());
                frame.extend_from_slice(&protocol.to_be_bytes());
                frame.extend_from_slice(buf);
                let _tx = iface
                    .common()
                    .try_acquire_tx()
                    .ok_or(SystemError::ENETDOWN)?;
                iface.raw_transmit(&frame)?;
                Ok(buf.len())
            }
        }
    }

    fn try_send(&self, buf: &[u8], dest: Option<SockAddrLl>) -> Result<usize, SystemError> {
        let prepared = self.prepare_send(buf.len(), dest)?;
        self.finish_send(prepared, buf)
    }

    pub(super) fn validate_packet_send_len(
        &self,
        len: usize,
        address: Option<&Endpoint>,
    ) -> Result<(), SystemError> {
        let dest = address.map(Self::endpoint_to_sockaddr).transpose()?;
        self.prepare_send(len, dest).map(|_| ())
    }

    pub(super) fn send_packet_user_buffer(
        &self,
        reader: &crate::syscall::user_access::UserBufferReader<'_>,
        len: usize,
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<usize, SystemError> {
        Self::validate_send_flags(flags)?;
        let dest = address
            .as_ref()
            .map(Self::endpoint_to_sockaddr)
            .transpose()?;
        let prepared = self.prepare_send(len, dest)?;
        let data = crate::net::socket::base::copy_user_buffer_to_vec(reader, len)?;
        self.finish_send(prepared, &data)
    }

    fn endpoint_to_sockaddr(address: &Endpoint) -> Result<SockAddrLl, SystemError> {
        let Endpoint::LinkLayer(ll) = address else {
            return Err(SystemError::EINVAL);
        };
        Ok(SockAddrLl {
            sll_family: 17,
            sll_protocol: ll.protocol,
            sll_ifindex: ll.interface as i32,
            sll_hatype: ll.hatype,
            sll_pkttype: ll.pkttype,
            sll_halen: ll.halen,
            sll_addr: ll.addr,
        })
    }
    fn validate_send_flags(flags: PMSG) -> Result<(), SystemError> {
        let allowed = PMSG::DONTWAIT | PMSG::DONTROUTE | PMSG::NOSIGNAL | PMSG::MORE;
        if !(flags & !allowed).is_empty() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        Ok(())
    }
    pub(super) fn send_packet(
        &self,
        buf: &[u8],
        flags: PMSG,
        dest: Option<SockAddrLl>,
    ) -> Result<usize, SystemError> {
        Self::validate_send_flags(flags)?;
        self.try_send(buf, dest)
    }
    pub(super) fn send_endpoint(
        &self,
        buf: &[u8],
        flags: PMSG,
        address: Endpoint,
    ) -> Result<usize, SystemError> {
        self.send_packet(buf, flags, Some(Self::endpoint_to_sockaddr(&address)?))
    }
    pub(super) fn send_packet_msg(
        &self,
        msg: &crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        Self::validate_send_flags(flags)?;
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let total = iovs.total_len();
        if total == usize::MAX {
            return Err(SystemError::EMSGSIZE);
        }
        // A partial user copy is not a shorter datagram.
        let dest = if !msg.msg_name.is_null() && msg.msg_namelen > 0 {
            let endpoint = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            Some(Self::endpoint_to_sockaddr(&endpoint)?)
        } else {
            None
        };
        let prepared = self.prepare_send(total, dest)?;
        let data = iovs.gather()?;
        if data.len() != total {
            return Err(SystemError::EFAULT);
        }
        self.finish_send(prepared, &data)
    }
}
