use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use system_error::SystemError;

use crate::filesystem::vfs::iov::IoVecs;
use crate::net::socket::endpoint::{Endpoint, LinkLayerEndpoint};
use crate::net::socket::unix::utils::CmsgBuffer;
use crate::net::socket::PMSG;

use super::uapi::{SOL_PACKET, TP_STATUS_USER, TP_STATUS_VLAN_TPID_VALID, TP_STATUS_VLAN_VALID};
use super::{
    eth_protocol, packet_option, PacketMetadata, PacketSocket, PacketSocketType, PacketType,
    ReceivedPacket, RingWriteResult, SockAddrLl, TpacketAuxdata,
};
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::fasync::FASYNC_POLL_IN;

struct ParsedFrame {
    src: [u8; 6],
    protocol: u16,
    vlan: Option<(u16, u16)>,
}

fn parse_frame(frame: &[u8]) -> Option<ParsedFrame> {
    if frame.len() < 14 {
        return None;
    }
    let src = frame[6..12].try_into().ok()?;
    let outer = u16::from_be_bytes([frame[12], frame[13]]);
    if outer == 0x8100 || outer == 0x88a8 {
        if frame.len() < 18 {
            return None;
        }
        let tci = u16::from_be_bytes([frame[14], frame[15]]);
        let protocol = u16::from_be_bytes([frame[16], frame[17]]);
        Some(ParsedFrame {
            src,
            protocol,
            vlan: Some((tci, outer)),
        })
    } else {
        Some(ParsedFrame {
            src,
            protocol: outer,
            vlan: None,
        })
    }
}

impl PacketSocket {
    pub(super) fn deliver_from_iface(&self, ifindex: u32, frame: &[u8], pkt_type: PacketType) {
        let (bound_ifindex, bound_protocol) = self.binding.load();
        if bound_protocol == 0 || (bound_ifindex != 0 && bound_ifindex != ifindex) {
            return;
        }
        let Some(ParsedFrame {
            src,
            protocol,
            vlan,
        }) = parse_frame(frame)
        else {
            return;
        };
        if bound_protocol != eth_protocol::ETH_P_ALL && bound_protocol != protocol {
            return;
        }

        // --- TPACKET ring buffer path ---
        // When an RX ring is active, deliver directly into the ring instead of
        // the rx_buffer queue.  Clone the Arc and release the outer lock so
        // concurrent delivers from other NICs are not blocked by setup/teardown.
        let ring_arc = self.rx_ring.lock().as_ref().cloned();
        if let Some(ring_arc) = ring_arc {
            let metadata = PacketMetadata {
                src_mac: src,
                protocol,
                ifindex,
                pkt_type,
                net_offset: 14,
                vlan_tci: vlan.map_or(0, |v| v.0),
                vlan_tpid: vlan.map_or(0, |v| v.1),
            };
            let mut ring = ring_arc.lock();
            match ring.write_frame(frame, &metadata) {
                RingWriteResult::Written => {
                    self.stats_packets.fetch_add(1, Ordering::Relaxed);
                    drop(ring);
                    self.wait_queue.wakeup(None);
                    let _ = EventPoll::wakeup_epoll(
                        self.epoll_items.as_ref(),
                        EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
                    );
                    self.fasync_items.send_sigio(FASYNC_POLL_IN);
                }
                RingWriteResult::Dropped => {
                    self.stats_drops.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        // Match Linux packet_rcv(): reject a socket whose receive memory is
        // already full before allocating and copying a private frame. The
        // atomic reservation below remains the final concurrent admission
        // check and permits Linux's one-packet bounded overshoot.
        if self.rx_buffer_bytes.load(Ordering::Acquire)
            >= self.recv_buffer_bytes.load(Ordering::Relaxed)
        {
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let visible_len = frame
            .len()
            .saturating_sub(if vlan.is_some() { 4 } else { 0 });
        let start = if self.sock_type == PacketSocketType::Raw {
            0
        } else {
            14
        };
        let data_len = visible_len - start;
        let metadata = PacketMetadata {
            src_mac: src,
            protocol,
            ifindex,
            pkt_type,
            net_offset: 14,
            vlan_tci: vlan.map_or(0, |v| v.0),
            vlan_tpid: vlan.map_or(0, |v| v.1),
        };
        let accounted_bytes = data_len.saturating_add(core::mem::size_of::<ReceivedPacket>());
        if self
            .rx_buffer_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                // Linux checks sk_rmem_alloc before charging the next skb, so
                // one final packet may take the queue over sk_rcvbuf.
                (used < self.recv_buffer_bytes.load(Ordering::Relaxed))
                    .then(|| used.checked_add(accounted_bytes))
                    .flatten()
            })
            .is_err()
        {
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut data = Vec::new();
        if data.try_reserve_exact(data_len).is_err() {
            self.rx_buffer_bytes
                .fetch_sub(accounted_bytes, Ordering::AcqRel);
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if vlan.is_some() {
            if self.sock_type == PacketSocketType::Raw {
                data.extend_from_slice(&frame[..12]);
                data.extend_from_slice(&frame[16..]);
            } else {
                data.extend_from_slice(&frame[18..]);
            }
        } else {
            data.extend_from_slice(&frame[start..]);
        }
        let packet = ReceivedPacket {
            data,
            metadata,
            accounted_bytes,
        };
        let mut queue = self.rx_buffer.lock();
        if queue.try_reserve(1).is_err() {
            self.rx_buffer_bytes
                .fetch_sub(accounted_bytes, Ordering::AcqRel);
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.push_back(packet);
        drop(queue);
        self.stats_packets.fetch_add(1, Ordering::Relaxed);
        self.wait_queue.wakeup(None);
    }
    pub(super) fn can_recv(&self) -> bool {
        // Ring mode: check for TP_STATUS_USER frames.
        if let Some(r) = self.rx_ring.lock().as_ref() {
            return r.lock().has_user_frames();
        }
        // Queue mode (default).
        !self.rx_buffer.lock().is_empty()
    }
    fn dequeue(&self, peek: bool) -> Result<ReceivedPacket, SystemError> {
        let mut queue = self.rx_buffer.lock();
        let packet = if peek {
            queue.front().cloned()
        } else {
            queue.pop_front()
        }
        .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        drop(queue);
        if !peek {
            self.rx_buffer_bytes
                .fetch_sub(packet.accounted_bytes, Ordering::AcqRel);
        }
        Ok(packet)
    }
    fn wait_dequeue(&self, flags: PMSG) -> Result<ReceivedPacket, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let peek = flags.contains(PMSG::PEEK);
        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            return self.dequeue(peek);
        }
        if let Some(timeout_ticks) = self.recv_timeout_ticks() {
            self.wait_queue
                .wait_until_timeout_ticks(|| self.dequeue(peek).ok(), timeout_ticks)
        } else {
            self.wait_queue
                .wait_until_interruptible(|| self.dequeue(peek).ok())
        }
    }
    pub(super) fn recv_packet(&self, buf: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        let packet = self.wait_dequeue(flags)?;
        let n = buf.len().min(packet.data.len());
        buf[..n].copy_from_slice(&packet.data[..n]);
        Ok(if flags.contains(PMSG::TRUNC) {
            packet.data.len()
        } else {
            n
        })
    }
    pub(super) fn recv_packet_from(
        &self,
        buf: &mut [u8],
        flags: PMSG,
    ) -> Result<(usize, Endpoint), SystemError> {
        let packet = self.wait_dequeue(flags)?;
        let n = buf.len().min(packet.data.len());
        buf[..n].copy_from_slice(&packet.data[..n]);
        let mut ll = LinkLayerEndpoint::new(packet.metadata.ifindex as usize);
        ll.addr[..6].copy_from_slice(&packet.metadata.src_mac);
        ll.protocol = packet.metadata.protocol;
        ll.hatype = 1;
        ll.pkttype = packet.metadata.pkt_type as u8;
        ll.halen = 6;
        Ok((
            if flags.contains(PMSG::TRUNC) {
                packet.data.len()
            } else {
                n
            },
            Endpoint::LinkLayer(ll),
        ))
    }
    pub(super) fn recv_packet_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let capacity = iovs.total_len();
        if capacity == usize::MAX {
            return Err(SystemError::EINVAL);
        }
        let packet = self.wait_dequeue(flags)?;
        let copy_len = capacity.min(packet.data.len());
        let written = iovs.scatter(&packet.data[..copy_len])?;
        if written != copy_len {
            return Err(SystemError::EFAULT);
        }
        if !msg.msg_name.is_null() {
            let full = core::mem::size_of::<SockAddrLl>();
            let n = (msg.msg_namelen as usize).min(full);
            let mut addr = [0; 8];
            addr[..6].copy_from_slice(&packet.metadata.src_mac);
            let sll = SockAddrLl {
                sll_family: 17,
                sll_protocol: packet.metadata.protocol.to_be(),
                sll_ifindex: packet.metadata.ifindex as i32,
                sll_hatype: 1,
                sll_pkttype: packet.metadata.pkt_type as u8,
                sll_halen: 6,
                sll_addr: addr,
            };
            let bytes = unsafe { core::slice::from_raw_parts(&sll as *const _ as *const u8, full) };
            let mut w = crate::syscall::user_access::UserBufferWriter::new(
                msg.msg_name as *mut u8,
                n,
                true,
            )?;
            w.buffer_protected(0)?.write_to_user(0, &bytes[..n])?;
            msg.msg_namelen = full as u32;
        } else {
            msg.msg_namelen = 0;
        }
        let control_len = msg.msg_controllen;
        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        if packet.data.len() > capacity {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }
        if self.options.read().auxdata {
            let vlan_status = if packet.metadata.vlan_tpid != 0 {
                TP_STATUS_VLAN_VALID | TP_STATUS_VLAN_TPID_VALID
            } else {
                0
            };
            let aux = TpacketAuxdata {
                tp_status: TP_STATUS_USER | vlan_status,
                tp_len: packet.data.len().min(u32::MAX as usize) as u32,
                tp_snaplen: packet.data.len().min(u32::MAX as usize) as u32,
                tp_mac: 0,
                tp_net: if self.sock_type == PacketSocketType::Raw {
                    packet.metadata.net_offset.min(u16::MAX as usize) as u16
                } else {
                    0
                },
                tp_vlan_tci: packet.metadata.vlan_tci,
                tp_vlan_tpid: packet.metadata.vlan_tpid,
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    &aux as *const _ as *const u8,
                    core::mem::size_of::<TpacketAuxdata>(),
                )
            };
            let mut off = 0;
            CmsgBuffer {
                ptr: msg.msg_control,
                len: control_len,
                write_off: &mut off,
            }
            .put(
                &mut msg.msg_flags,
                SOL_PACKET,
                packet_option::PACKET_AUXDATA as i32,
                bytes.len(),
                bytes,
            )?;
            msg.msg_controllen = off;
        }
        Ok(if flags.contains(PMSG::TRUNC) {
            packet.data.len()
        } else {
            copy_len
        })
    }
}
