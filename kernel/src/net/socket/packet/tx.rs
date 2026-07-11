use alloc::vec::Vec;
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::filesystem::vfs::iov::IoVecs;
use crate::net::posix::SockAddr;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::PMSG;

use super::{PacketSocket, PacketSocketType, SockAddrLl};

impl PacketSocket {
    pub(super) fn validate_packet_len(&self, len: usize) -> Result<(), SystemError> {
        if len > u16::MAX as usize {
            return Err(SystemError::EMSGSIZE);
        }
        Ok(())
    }
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
    fn try_send(&self, buf: &[u8], dest: Option<SockAddrLl>) -> Result<usize, SystemError> {
        self.validate_packet_len(buf.len())?;
        let iface = self.destination_iface(dest.as_ref())?;
        match self.sock_type {
            PacketSocketType::Raw => {
                if buf.len() < 14 {
                    return Err(SystemError::EINVAL);
                }
                iface.raw_transmit(buf)?;
                Ok(buf.len())
            }
            PacketSocketType::Dgram => {
                let addr = dest.ok_or(SystemError::EDESTADDRREQ)?;
                if addr.sll_halen < 6 {
                    return Err(SystemError::EINVAL);
                }
                let total = 14usize
                    .checked_add(buf.len())
                    .ok_or(SystemError::EMSGSIZE)?;
                self.validate_packet_len(total)?;
                let protocol = if addr.sll_protocol != 0 {
                    addr.sll_protocol
                } else {
                    self.binding.load().1
                };
                if protocol == 0 {
                    return Err(SystemError::EINVAL);
                }
                let mut frame = Vec::new();
                frame
                    .try_reserve_exact(total)
                    .map_err(|_| SystemError::ENOMEM)?;
                frame.extend_from_slice(&addr.sll_addr[..6]);
                frame.extend_from_slice(iface.mac().as_bytes());
                frame.extend_from_slice(&protocol.to_be_bytes());
                frame.extend_from_slice(buf);
                iface.raw_transmit(&frame)?;
                Ok(buf.len())
            }
        }
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
        let Endpoint::LinkLayer(ll) = address else {
            return Err(SystemError::EINVAL);
        };
        self.send_packet(
            buf,
            flags,
            Some(SockAddrLl {
                sll_family: 17,
                sll_protocol: ll.protocol,
                sll_ifindex: ll.interface as i32,
                sll_hatype: ll.hatype,
                sll_pkttype: ll.pkttype,
                sll_halen: ll.halen,
                sll_addr: ll.addr,
            }),
        )
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
        self.validate_packet_len(total)?;
        let data = iovs.gather()?;
        // A partial user copy is not a shorter datagram.
        if data.len() != total {
            return Err(SystemError::EFAULT);
        }
        let dest = if !msg.msg_name.is_null() && msg.msg_namelen > 0 {
            let endpoint = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            let Endpoint::LinkLayer(ll) = endpoint else {
                return Err(SystemError::EINVAL);
            };
            Some(SockAddrLl {
                sll_family: 17,
                sll_protocol: ll.protocol,
                sll_ifindex: ll.interface as i32,
                sll_hatype: ll.hatype,
                sll_pkttype: ll.pkttype,
                sll_halen: ll.halen,
                sll_addr: ll.addr,
            })
        } else {
            None
        };
        self.try_send(&data, dest)
    }
}
