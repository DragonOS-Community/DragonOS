//! Bounded inet extended-error storage and Linux MSG_ERRQUEUE serialization.
use crate::{
    filesystem::vfs::iov::IoVecs,
    net::{
        posix::{MsgHdr, SockAddr},
        socket::{endpoint::Endpoint, unix::utils::CmsgBuffer, IpOption, PIPV6, PMSG, PSOL},
    },
};
use alloc::{collections::VecDeque, vec::Vec};
use smoltcp::wire::{IpAddress, IpEndpoint};
use system_error::SystemError;

pub(crate) const SO_EE_ORIGIN_LOCAL: u8 = 1;
pub(crate) const SO_EE_ORIGIN_ICMP: u8 = 2;
pub(crate) const SO_EE_ORIGIN_ICMP6: u8 = 3;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SockExtendedErr {
    pub(crate) ee_errno: u32,
    pub(crate) ee_origin: u8,
    pub(crate) ee_type: u8,
    pub(crate) ee_code: u8,
    pub(crate) ee_pad: u8,
    pub(crate) ee_info: u32,
    pub(crate) ee_data: u32,
}

#[derive(Debug)]
pub(crate) struct ErrorQueueEntry {
    pub(crate) error: SockExtendedErr,
    pub(crate) offender: Option<IpAddress>,
    pub(crate) destination: IpEndpoint,
    pub(crate) payload: Vec<u8>,
    pub(crate) ipv6: bool,
    pub(crate) ingress_ifindex: u32,
    pub(crate) packet: Option<ErrorPacketMetadata>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ErrorPacketMetadata {
    pub(crate) ttl: u8,
    pub(crate) tos: u8,
    pub(crate) local_address: IpAddress,
}

#[derive(Default)]
pub(crate) struct ErrorCmsgOptions {
    pub(crate) pktinfo: bool,
    pub(crate) ttl: bool,
    pub(crate) tos: bool,
}

impl ErrorQueueEntry {
    fn cost(&self) -> usize {
        self.payload.len().saturating_add(256)
    }

    pub(crate) fn recv_msg_with_options(
        self,
        msg: &mut MsgHdr,
        options: ErrorCmsgOptions,
    ) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let copied = iovs.total_len().min(self.payload.len());
        iovs.scatter_exact(&self.payload[..copied])?;
        msg.msg_flags = PMSG::ERRQUEUE.bits() as i32;
        if copied < self.payload.len() {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }
        if !msg.msg_name.is_null() {
            let supplied_name_len = msg.msg_namelen;
            msg.msg_namelen = Endpoint::Ip(self.destination)
                .write_to_user_msghdr(msg.msg_name, msg.msg_namelen)?;
            if supplied_name_len >= 28
                && matches!(self.destination.addr,
                IpAddress::Ipv6(address) if address.is_unicast_link_local())
            {
                let mut writer = crate::syscall::user_access::UserBufferWriter::new(
                    msg.msg_name.cast::<u8>(),
                    28,
                    true,
                )?;
                writer
                    .buffer_protected(0)?
                    .write_to_user(24, &self.ingress_ifindex.to_ne_bytes())?;
            }
        } else {
            msg.msg_namelen = 0;
        }
        // Zero-initialize all ABI padding, including AF_UNSPEC for local errors.
        let mut data = [0u8; 44];
        data[..4].copy_from_slice(&self.error.ee_errno.to_ne_bytes());
        data[4] = self.error.ee_origin;
        data[5] = self.error.ee_type;
        data[6] = self.error.ee_code;
        data[8..12].copy_from_slice(&self.error.ee_info.to_ne_bytes());
        data[12..16].copy_from_slice(&self.error.ee_data.to_ne_bytes());
        let addr_len = if self.ipv6 { 28 } else { 16 };
        if let Some(address) = self.offender {
            let sockaddr = SockAddr::from(Endpoint::Ip(IpEndpoint::new(address, 0)));
            let bytes = unsafe {
                core::slice::from_raw_parts((&sockaddr as *const SockAddr).cast::<u8>(), addr_len)
            };
            data[16..16 + addr_len].copy_from_slice(bytes);
            if matches!(address, IpAddress::Ipv6(address) if address.is_unicast_link_local()) {
                data[40..44].copy_from_slice(&self.ingress_ifindex.to_ne_bytes());
            }
        }
        let mut written = 0;
        let mut control = CmsgBuffer {
            ptr: msg.msg_control,
            len: msg.msg_controllen,
            write_off: &mut written,
        };
        if let Some(packet) = self.packet {
            let packet_ipv6 = matches!(packet.local_address, IpAddress::Ipv6(_));
            let level = if packet_ipv6 {
                PSOL::IPV6 as i32
            } else {
                PSOL::IP as i32
            };
            if options.pktinfo {
                let mut info = [0u8; 20];
                let len = match packet.local_address {
                    IpAddress::Ipv4(address) => {
                        info[..4].copy_from_slice(&self.ingress_ifindex.to_ne_bytes());
                        info[4..8].copy_from_slice(&address.octets());
                        info[8..12].copy_from_slice(&address.octets());
                        12
                    }
                    IpAddress::Ipv6(address) => {
                        info[..16].copy_from_slice(&address.octets());
                        info[16..20].copy_from_slice(&self.ingress_ifindex.to_ne_bytes());
                        20
                    }
                };
                control.put(
                    &mut msg.msg_flags,
                    level,
                    if packet_ipv6 {
                        PIPV6::PKTINFO as i32
                    } else {
                        IpOption::PKTINFO as i32
                    },
                    len,
                    &info[..len],
                )?;
            }
            if options.ttl {
                let ttl = (packet.ttl as i32).to_ne_bytes();
                control.put(
                    &mut msg.msg_flags,
                    level,
                    if packet_ipv6 {
                        PIPV6::HOPLIMIT as i32
                    } else {
                        IpOption::TTL as i32
                    },
                    4,
                    &ttl,
                )?;
            }
            if options.tos {
                let tos = (packet.tos as i32).to_ne_bytes();
                let len = if packet_ipv6 { 4 } else { 1 };
                let data = if packet_ipv6 {
                    &tos[..]
                } else {
                    core::slice::from_ref(&packet.tos)
                };
                control.put(
                    &mut msg.msg_flags,
                    level,
                    if packet_ipv6 {
                        PIPV6::TCLASS as i32
                    } else {
                        IpOption::TOS as i32
                    },
                    len,
                    data,
                )?;
            }
        }
        control.put(
            &mut msg.msg_flags,
            if self.ipv6 {
                PSOL::IPV6 as i32
            } else {
                PSOL::IP as i32
            },
            if self.ipv6 {
                PIPV6::RECVERR as i32
            } else {
                IpOption::RECVERR as i32
            },
            16 + addr_len,
            &data[..16 + addr_len],
        )?;
        msg.msg_controllen = written;
        // Extended-error reception returns copied even for input MSG_TRUNC.
        Ok(copied)
    }
}

#[derive(Debug, Default)]
pub(crate) struct ErrorQueue {
    entries: VecDeque<ErrorQueueEntry>,
    bytes: usize,
}

impl ErrorQueue {
    pub(crate) fn push(&mut self, entry: ErrorQueueEntry, rcvbuf: usize) -> bool {
        let cost = entry.cost();
        if self.entries.len() >= 64
            || cost > rcvbuf.min(65_536).saturating_sub(self.bytes)
            || self.entries.try_reserve(1).is_err()
        {
            return false;
        }
        self.bytes += cost;
        self.entries.push_back(entry);
        true
    }
    pub(crate) fn pop(&mut self) -> Option<ErrorQueueEntry> {
        let entry = self.entries.pop_front()?;
        self.bytes = self.bytes.saturating_sub(entry.cost());
        Some(entry)
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(crate) fn pending_after_pop(&self, removed: &ErrorQueueEntry, previous: i32) -> i32 {
        let is_icmp = |entry: &ErrorQueueEntry| {
            matches!(
                entry.error.ee_origin,
                SO_EE_ORIGIN_ICMP | SO_EE_ORIGIN_ICMP6
            )
        };
        if let Some(next) = self.entries.front().filter(|entry| is_icmp(entry)) {
            next.error.ee_errno as i32
        } else if is_icmp(removed) {
            0
        } else {
            previous
        }
    }
}
