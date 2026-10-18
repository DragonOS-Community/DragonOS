use smoltcp::wire::{IpAddress, IpProtocol, IpVersion};
use system_error::SystemError;

use crate::net::socket::recv_buffer::{ReceiveAttempt, RecvBuffer};
use crate::net::socket::unix::utils::CmsgBuffer;
use crate::net::socket::utils::extract_src_addr_from_ip_header;
use crate::net::socket::utils::{IPV4_MIN_HEADER_LEN, IPV6_HEADER_LEN};
use crate::net::socket::{IpOption, PIPV6, PMSG, PSOL};

use super::inner;
use super::inner::RawInner;
use super::loopback::loopback_rx_mem_cost;
use super::packet::ipv6_udp_checksum;
use super::{RawSocket, RawSocketOptions};

/// Scalar receives copy a prefix directly; recvmsg stages the actual complete
/// IP packet for checksum verification and ancillary header parsing.
enum RawReceiveTarget<'a> {
    Buffer(&'a mut [u8]),
    Staged(&'a mut RecvBuffer),
}

impl RawReceiveTarget<'_> {
    fn needs_buffer(&self, packet_len: usize) -> bool {
        matches!(self, Self::Staged(buffer) if buffer.capacity() < packet_len)
    }

    fn copy_packet(&mut self, packet: &[u8]) -> usize {
        let output = match self {
            Self::Buffer(buffer) => &mut **buffer,
            Self::Staged(buffer) => buffer.bytes_mut(packet.len()),
        };
        let copied = output.len().min(packet.len());
        output[..copied].copy_from_slice(&packet[..copied]);
        copied
    }
}

struct RawReceived {
    copied: usize,
    original_len: usize,
    source: IpAddress,
    ifindex: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct InPktInfo {
    ipi_ifindex: i32,
    ipi_spec_dst: u32,
    ipi_addr: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct In6PktInfo {
    ipi6_addr: [u8; 16],
    ipi6_ifindex: u32,
}

impl RawSocket {
    /// 尝试接收数据包
    ///
    /// # 返回
    /// - `Ok((size, src_addr))`: 接收到的数据大小和源地址
    /// - `Err(SystemError::EAGAIN_OR_EWOULDBLOCK)`: 没有数据可读
    pub fn try_recv(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpAddress), SystemError> {
        self.try_recv_with_ifindex(buf)
            .map(|(size, source, _)| (size, source))
    }

    fn try_recv_with_ifindex(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, IpAddress, u32), SystemError> {
        match self.try_recv_packet(&mut RawReceiveTarget::Buffer(buf), false)? {
            ReceiveAttempt::Ready(received) => {
                Ok((received.copied, received.source, received.ifindex))
            }
            ReceiveAttempt::NeedBuffer(_) => unreachable!("scalar storage is already provided"),
        }
    }

    fn packet_source(&self, packet: &[u8]) -> IpAddress {
        extract_src_addr_from_ip_header(packet, self.ip_version).unwrap_or(match self.ip_version {
            IpVersion::Ipv4 => IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
            IpVersion::Ipv6 => IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED),
        })
    }

    /// Validate the borrowed complete packet, before truncation for any caller.
    /// The same predicate serves scalar and vectored receive transactions.
    fn accepts_packet(
        &self,
        packet: &[u8],
        local: Option<IpAddress>,
        options: &RawSocketOptions,
    ) -> bool {
        if let Some(local) = local.filter(|address| !address.is_unspecified()) {
            if inner::extract_dst_addr_from_ip_header(packet, self.ip_version)
                .is_some_and(|destination| destination != local)
            {
                return false;
            }
        }
        if self.ip_version == IpVersion::Ipv6 && self.protocol == IpProtocol::Udp {
            let offset = options.ipv6_checksum;
            if offset >= 0 {
                let offset = offset as usize;
                let Some(payload) = packet.get(IPV6_HEADER_LEN..) else {
                    return false;
                };
                let Some(checksum) = payload.get(offset..offset.saturating_add(2)) else {
                    return false;
                };
                let got = u16::from_be_bytes([checksum[0], checksum[1]]);
                if got == 0 || ipv6_udp_checksum(packet, offset) != Some(got) {
                    return false;
                }
            }
        }
        if self.protocol == IpProtocol::Icmp {
            if let Some(&kind) = packet.get(self.get_ip_header_len(packet)) {
                if options.icmp_filter.should_filter(kind) {
                    return false;
                }
            }
        }
        true
    }

    fn try_recv_packet(
        &self,
        target: &mut RawReceiveTarget<'_>,
        peek: bool,
    ) -> Result<ReceiveAttempt<RawReceived>, SystemError> {
        self.take_pending_error()?;
        {
            let mut queue = self.loopback_rx.lock();
            if let Some(packet) = queue.pkts.front() {
                // Ingress already applied address, ICMP and checksum admission.
                // Do not reinterpret queued packets after socket options change.
                if target.needs_buffer(packet.bytes.len()) {
                    return Ok(ReceiveAttempt::NeedBuffer(packet.bytes.len()));
                }
                if peek {
                    let received = RawReceived {
                        copied: target.copy_packet(&packet.bytes),
                        original_len: packet.bytes.len(),
                        source: self.packet_source(&packet.bytes),
                        ifindex: packet.ingress_ifindex,
                    };
                    return Ok(ReceiveAttempt::Ready(received));
                }
                // Ownership fixes payload and metadata identity; keep the large
                // copy and packet destruction outside the queue's critical section.
                let packet = queue.pkts.pop_front().unwrap();
                queue.bytes = queue
                    .bytes
                    .saturating_sub(loopback_rx_mem_cost(packet.bytes.len()));
                drop(queue);
                return Ok(ReceiveAttempt::Ready(RawReceived {
                    copied: target.copy_packet(&packet.bytes),
                    original_len: packet.bytes.len(),
                    source: self.packet_source(&packet.bytes),
                    ifindex: packet.ingress_ifindex,
                }));
            }
        }

        // Identity/options guards never span a transport attachment operation.
        let local = self.addresses.read().local;
        let options = self.options.read().clone();
        const MAX_FILTER_RETRIES: usize = 64;
        let inner_guard = self.inner.read();
        let bound = match inner_guard.as_ref() {
            Some(RawInner::Bound(bound)) | Some(RawInner::Wildcard(bound)) => bound,
            Some(RawInner::Unbound(_)) | None => return Err(SystemError::ENOTCONN),
        };
        let ifindex = bound.inner().iface().nic_id() as u32;
        let result = bound.with_mut_socket(|socket| {
            for _ in 0..MAX_FILTER_RETRIES {
                let packet = socket
                    .peek()
                    .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
                if !self.accepts_packet(packet, local, &options) {
                    // Invalid packets must not permanently block a PEEK reader.
                    let _ = socket.recv();
                    continue;
                }
                if target.needs_buffer(packet.len()) {
                    return Ok(ReceiveAttempt::NeedBuffer(packet.len()));
                }
                let received = RawReceived {
                    copied: target.copy_packet(packet),
                    original_len: packet.len(),
                    source: self.packet_source(packet),
                    ifindex,
                };
                if !peek {
                    socket
                        .recv()
                        .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
                }
                return Ok(ReceiveAttempt::Ready(received));
            }
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        });
        let iface = bound.inner().iface().clone();
        drop(inner_guard);
        if matches!(&result, Ok(ReceiveAttempt::Ready(_))) {
            iface.poll();
        }
        result
    }

    fn recv_packet_staged(
        &self,
        buffer: &mut RecvBuffer,
        flags: PMSG,
    ) -> Result<RawReceived, SystemError> {
        let nonblock = self.is_nonblock() || flags.contains(PMSG::DONTWAIT);
        loop {
            match self.try_recv_packet(
                &mut RawReceiveTarget::Staged(buffer),
                flags.contains(PMSG::PEEK),
            ) {
                Ok(ReceiveAttempt::Ready(received)) => return Ok(received),
                Ok(ReceiveAttempt::NeedBuffer(required)) => buffer.prepare(required)?,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) if !nonblock => {
                    self.wait_queue.wait_event_io_interruptible_timeout(
                        || self.can_recv(),
                        self.recv_timeout(),
                    )?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Linux 语义：IPv4 raw socket 的 recv/recvfrom 返回整个 IPv4 包（含 IPv4 头）；
    /// IPv6 raw socket 默认只返回 payload（不含 IPv6 固定头）。
    pub(super) fn try_recv_user(
        &self,
        user_buf: &mut [u8],
    ) -> Result<(usize, crate::net::socket::endpoint::InetEndpoint), SystemError> {
        match self.ip_version {
            IpVersion::Ipv4 => {
                self.try_recv_with_ifindex(user_buf)
                    .map(|(size, source, ifindex)| {
                        (
                            size,
                            crate::net::socket::endpoint::InetEndpoint::from(
                                smoltcp::wire::IpEndpoint::new(source, 0),
                            )
                            .with_scope(ifindex),
                        )
                    })
            }
            IpVersion::Ipv6 => {
                let mut buffer = RecvBuffer::default();
                let received = self.recv_packet_staged(&mut buffer, PMSG::DONTWAIT)?;
                let recv_size = received.copied;
                let src_addr = received.source;
                let ifindex = received.ifindex;
                let tmp = buffer.bytes(recv_size);
                let start = IPV6_HEADER_LEN.min(recv_size);
                let payload = &tmp[start..recv_size];
                let to_copy = core::cmp::min(payload.len(), user_buf.len());
                user_buf[..to_copy].copy_from_slice(&payload[..to_copy]);
                Ok((
                    to_copy,
                    crate::net::socket::endpoint::InetEndpoint::from(
                        smoltcp::wire::IpEndpoint::new(src_addr, 0),
                    )
                    .with_scope(ifindex),
                ))
            }
        }
    }

    /// 填充 peer 地址到 msg_name
    ///
    /// Linux 语义：peer port 固定为 0
    fn fill_peer_addr(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        src_addr: IpAddress,
        ifindex: u32,
    ) -> Result<(), SystemError> {
        if msg.msg_name.is_null() {
            return Ok(());
        }
        let endpoint = crate::net::socket::endpoint::InetEndpoint::from(
            smoltcp::wire::IpEndpoint::new(src_addr, 0),
        )
        .with_scope(ifindex);
        msg.msg_namelen = crate::net::socket::endpoint::Endpoint::ip(endpoint)
            .write_to_user_msghdr(msg.msg_name, msg.msg_namelen)?;
        Ok(())
    }

    /// 构建 IPv4 接收控制消息
    fn build_ipv4_cmsgs(
        &self,
        cmsg_buf: &mut CmsgBuffer,
        msg_flags: &mut i32,
        packet: &[u8],
        recv_size: usize,
        options: &RawSocketOptions,
        ingress_ifindex: u32,
    ) -> Result<(), SystemError> {
        if recv_size < IPV4_MIN_HEADER_LEN {
            return Ok(());
        }

        let tos = packet[1];
        let ttl = packet[8];
        let dst = u32::from_be_bytes([packet[16], packet[17], packet[18], packet[19]]);

        // IP_PKTINFO -> in_pktinfo
        if options.recv_pktinfo_v4 {
            let pktinfo = InPktInfo {
                ipi_ifindex: ingress_ifindex as i32,
                ipi_spec_dst: dst.to_be(),
                ipi_addr: dst.to_be(),
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&pktinfo as *const InPktInfo) as *const u8,
                    core::mem::size_of::<InPktInfo>(),
                )
            };
            cmsg_buf.put(
                msg_flags,
                PSOL::IP as i32,
                IpOption::PKTINFO as i32,
                core::mem::size_of::<InPktInfo>(),
                bytes,
            )?;
        }

        // IP_RECVTOS
        if options.recv_tos {
            cmsg_buf.put(msg_flags, PSOL::IP as i32, IpOption::TOS as i32, 1, &[tos])?;
        }

        // IP_RECVTTL
        if options.recv_ttl {
            let v = (ttl as i32).to_ne_bytes();
            cmsg_buf.put(
                msg_flags,
                PSOL::IP as i32,
                IpOption::TTL as i32,
                core::mem::size_of::<i32>(),
                &v,
            )?;
        }

        Ok(())
    }

    /// 构建 IPv6 接收控制消息
    fn build_ipv6_cmsgs(
        &self,
        cmsg_buf: &mut CmsgBuffer,
        msg_flags: &mut i32,
        packet: &[u8],
        recv_size: usize,
        options: &RawSocketOptions,
        ingress_ifindex: u32,
    ) -> Result<(), SystemError> {
        if recv_size < IPV6_HEADER_LEN {
            return Ok(());
        }

        let traffic_class = ((packet[0] & 0x0f) << 4) | (packet[1] >> 4);
        let hop_limit = packet[7];
        let dst = &packet[24..40];

        // IPV6_RECVPKTINFO -> in6_pktinfo
        if options.recv_pktinfo_v6 {
            let mut pktinfo = In6PktInfo::default();
            pktinfo.ipi6_addr.copy_from_slice(dst);
            pktinfo.ipi6_ifindex = ingress_ifindex;
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&pktinfo as *const In6PktInfo) as *const u8,
                    core::mem::size_of::<In6PktInfo>(),
                )
            };
            cmsg_buf.put(
                msg_flags,
                PSOL::IPV6 as i32,
                PIPV6::PKTINFO as i32,
                core::mem::size_of::<In6PktInfo>(),
                bytes,
            )?;
        }

        // IPV6_RECVTCLASS
        if options.recv_tclass {
            let v = (traffic_class as i32).to_ne_bytes();
            cmsg_buf.put(
                msg_flags,
                PSOL::IPV6 as i32,
                PIPV6::TCLASS as i32,
                core::mem::size_of::<i32>(),
                &v,
            )?;
        }

        // IPV6_RECVHOPLIMIT
        if options.recv_hoplimit {
            let v = (hop_limit as i32).to_ne_bytes();
            cmsg_buf.put(
                msg_flags,
                PSOL::IPV6 as i32,
                PIPV6::HOPLIMIT as i32,
                core::mem::size_of::<i32>(),
                &v,
            )?;
        }

        Ok(())
    }

    /// 构建接收控制消息 (cmsg)
    ///
    /// 根据 socket 选项和接收的 IP 头信息，构建相应的控制消息
    fn build_recv_cmsgs(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        packet: &[u8],
        recv_size: usize,
        ingress_ifindex: u32,
    ) -> Result<usize, SystemError> {
        let mut write_off = 0usize;
        let mut cmsg_buf = CmsgBuffer {
            ptr: msg.msg_control,
            len: msg.msg_controllen,
            write_off: &mut write_off,
        };

        let options = self.options.read().clone();

        match self.ip_version {
            IpVersion::Ipv4 => self.build_ipv4_cmsgs(
                &mut cmsg_buf,
                &mut msg.msg_flags,
                packet,
                recv_size,
                &options,
                ingress_ifindex,
            )?,
            IpVersion::Ipv6 => self.build_ipv6_cmsgs(
                &mut cmsg_buf,
                &mut msg.msg_flags,
                packet,
                recv_size,
                &options,
                ingress_ifindex,
            )?,
        }

        Ok(write_off)
    }

    pub fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: crate::net::socket::PMSG,
        iovs: &crate::syscall::iov::IoVecs,
    ) -> Result<usize, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if flags.contains(crate::net::socket::PMSG::ERRQUEUE) {
            let entry = {
                let mut errors = self.errors.lock();
                let entry = errors.1.pop().ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
                errors.0 = errors.1.pending_after_pop(&entry, errors.0);
                entry
            };
            let options = self.options.read().clone();
            let ipv6 = self.ip_version == IpVersion::Ipv6;
            return entry.recv_msg_with_options(
                msg,
                super::super::common::error_queue::ErrorCmsgOptions {
                    pktinfo: if ipv6 {
                        options.recv_pktinfo_v6
                    } else {
                        options.recv_pktinfo_v4
                    },
                    ttl: if ipv6 {
                        options.recv_hoplimit
                    } else {
                        options.recv_ttl
                    },
                    tos: if ipv6 {
                        options.recv_tclass
                    } else {
                        options.recv_tos
                    },
                },
                iovs,
            );
        }

        let user_len = iovs.total_len();
        let mut buffer = RecvBuffer::default();
        let received = self.recv_packet_staged(&mut buffer, flags)?;
        let recv_size = received.copied;
        let src_addr = received.source;
        let ingress_ifindex = received.ifindex;
        let tmp = buffer.bytes(recv_size);

        // IPv4: 向用户返回整个 IPv4 包(含 IPv4 头)。
        // IPv6: 向用户仅返回 payload（不含 IPv6 固定头）。
        let (user_data, full_user_len) = match self.ip_version {
            IpVersion::Ipv4 => (&tmp[..recv_size], recv_size),
            IpVersion::Ipv6 => {
                let start = IPV6_HEADER_LEN.min(recv_size);
                let payload = &tmp[start..recv_size];
                (payload, payload.len())
            }
        };
        let user_recv_size = full_user_len.min(user_len);
        iovs.scatter_exact(&user_data[..user_recv_size])?;

        msg.msg_flags = if user_recv_size < full_user_len {
            PMSG::TRUNC.bits() as i32
        } else {
            0
        };

        // 填充 peer 地址
        self.fill_peer_addr(msg, src_addr, ingress_ifindex)?;

        // 构建控制消息
        msg.msg_controllen = self.build_recv_cmsgs(msg, tmp, recv_size, ingress_ifindex)?;

        Ok(if flags.contains(PMSG::TRUNC) {
            match self.ip_version {
                IpVersion::Ipv4 => received.original_len,
                IpVersion::Ipv6 => received.original_len.saturating_sub(IPV6_HEADER_LEN),
            }
        } else {
            user_recv_size
        })
    }

    pub fn validate_sendto_addr(
        &self,
        addr: *const crate::net::posix::SockAddr,
        addrlen: u32,
    ) -> Result<(), SystemError> {
        // Linux also accepts RFC2133's 24-byte layout without a scope tail.
        if !addr.is_null() && self.is_ipv6() && addrlen < 24 {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    pub fn recv(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        if self.is_nonblock() || flags.contains(crate::net::socket::PMSG::DONTWAIT) {
            self.try_recv_user(buffer).map(|(len, _)| len)
        } else {
            loop {
                match self.try_recv_user(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || self.can_recv(),
                            self.recv_timeout(),
                        )?;
                    }
                    result => return result.map(|(len, _)| len),
                }
            }
        }
    }

    pub fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
        _address: Option<crate::net::socket::endpoint::Endpoint>,
    ) -> Result<(usize, crate::net::socket::endpoint::Endpoint), SystemError> {
        // Linux 语义：raw socket 的 recvfrom(2) 返回的 sockaddr_{in,in6}.port 为 0。
        if self.is_nonblock() || flags.contains(crate::net::socket::PMSG::DONTWAIT) {
            self.try_recv_user(buffer)
                .map(|(len, addr)| (len, crate::net::socket::endpoint::Endpoint::ip(addr)))
        } else {
            loop {
                match self.try_recv_user(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || self.can_recv(),
                            self.recv_timeout(),
                        )?;
                    }
                    result => {
                        return result.map(|(len, addr)| {
                            (len, crate::net::socket::endpoint::Endpoint::ip(addr))
                        })
                    }
                }
            }
        }
    }
}
