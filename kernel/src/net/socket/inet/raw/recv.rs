use alloc::vec;

use smoltcp::wire::{IpAddress, IpProtocol, IpVersion};
use system_error::SystemError;

use crate::filesystem::vfs::iov::IoVecs;
use crate::net::posix::{SockAddrIn, SockAddrIn6};
use crate::net::socket::unix::utils::CmsgBuffer;
use crate::net::socket::utils::{IPV4_MIN_HEADER_LEN, IPV6_HEADER_LEN};
use crate::net::socket::{IpOption, PIPV6, PSOL};
use crate::syscall::user_access::UserBufferWriter;

use super::inner;
use super::inner::RawInner;
use super::loopback::loopback_rx_mem_cost;
use super::packet::ipv6_udp_checksum;
use super::{RawSocket, RawSocketOptions};

// 接收过滤重试时的宏：检查重试次数，超过限制则返回 EAGAIN
macro_rules! filter_retry_or_break {
    ($retries:expr, $max:expr, $result:expr) => {{
        $retries += 1;
        if $retries >= $max {
            $result = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            break;
        }
        continue;
    }};
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
        // 先消费回环注入队列（保留原始头字段，并实现 SO_RCVBUF 语义）。
        if let Some(pkt) = {
            let mut q = self.loopback_rx.lock_irqsave();
            let pkt = q.pkts.pop_front();
            if let Some(ref p) = pkt {
                q.bytes = q.bytes.saturating_sub(loopback_rx_mem_cost(p.len()));
            }
            pkt
        } {
            let len = pkt.len().min(buf.len());
            buf[..len].copy_from_slice(&pkt[..len]);
            let src_addr = match self.ip_version {
                IpVersion::Ipv4 => {
                    if pkt.len() >= 20 {
                        IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                            pkt[12], pkt[13], pkt[14], pkt[15],
                        ))
                    } else {
                        IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED)
                    }
                }
                IpVersion::Ipv6 => {
                    if pkt.len() >= 40 {
                        let b: [u8; 16] = pkt[8..24].try_into().unwrap_or([0; 16]);
                        IpAddress::Ipv6(smoltcp::wire::Ipv6Address::new(
                            u16::from_be_bytes([b[0], b[1]]),
                            u16::from_be_bytes([b[2], b[3]]),
                            u16::from_be_bytes([b[4], b[5]]),
                            u16::from_be_bytes([b[6], b[7]]),
                            u16::from_be_bytes([b[8], b[9]]),
                            u16::from_be_bytes([b[10], b[11]]),
                            u16::from_be_bytes([b[12], b[13]]),
                            u16::from_be_bytes([b[14], b[15]]),
                        ))
                    } else {
                        IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED)
                    }
                }
            };
            return Ok((len, src_addr));
        }

        let inner_guard = self.inner.read();
        match inner_guard.as_ref() {
            None => Err(SystemError::ENOTCONN),
            Some(RawInner::Bound(bound)) => {
                // 接收数据，并按 Linux 语义应用 bind(2) 的目的地址过滤。
                // gVisor raw_socket_test: RawSocketTest.BindReceive
                // 添加最大重试次数限制，避免大量不匹配包导致的 busy-wait。
                const MAX_FILTER_RETRIES: usize = 64;
                let mut result;
                let mut retries = 0;
                loop {
                    result = bound.try_recv(buf, self.ip_version);
                    match &result {
                        Ok((size, _src_addr)) => {
                            if let Some(local) = bound.local_addr() {
                                if let Some(dst) = inner::extract_dst_addr_from_ip_header(
                                    &buf[..(*size).min(buf.len())],
                                    self.ip_version,
                                ) {
                                    if dst != local {
                                        // 丢弃不匹配的包，继续尝试读取下一包。
                                        filter_retry_or_break!(retries, MAX_FILTER_RETRIES, result);
                                    }
                                }
                            }

                            // Linux 语义：若启用了 IPV6_CHECKSUM（用于 UDP），则在接收路径校验校验和。
                            if self.ip_version == IpVersion::Ipv6
                                && self.protocol == IpProtocol::Udp
                            {
                                let off = self.options.read().ipv6_checksum;
                                if off >= 0 {
                                    let size = (*size).min(buf.len());
                                    let packet = &buf[..size];
                                    let off = off as usize;
                                    if packet.len() < 40 {
                                        filter_retry_or_break!(retries, MAX_FILTER_RETRIES, result);
                                    }
                                    let payload = &packet[40..];
                                    if off + 2 > payload.len() {
                                        filter_retry_or_break!(retries, MAX_FILTER_RETRIES, result);
                                    }
                                    let got = u16::from_be_bytes([payload[off], payload[off + 1]]);
                                    // IPv6/UDP: checksum 不能为 0。
                                    if got == 0 {
                                        filter_retry_or_break!(retries, MAX_FILTER_RETRIES, result);
                                    }
                                    match ipv6_udp_checksum(packet, off) {
                                        Some(expect) if expect == got => {}
                                        _ => {
                                            filter_retry_or_break!(
                                                retries,
                                                MAX_FILTER_RETRIES,
                                                result
                                            );
                                        }
                                    }
                                }
                            }
                            break;
                        }
                        Err(_) => break,
                    }
                }

                // 应用 ICMP_FILTER
                if let Ok((size, src_addr)) = &result {
                    if self.protocol == IpProtocol::Icmp && *size > 0 {
                        // 获取 ICMP type (IP 头后第一个字节)
                        let ip_header_len = self.get_ip_header_len(buf);
                        if buf.len() > ip_header_len {
                            let icmp_type = buf[ip_header_len];
                            if self.options.read().icmp_filter.should_filter(icmp_type) {
                                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                            }
                        }
                    }
                    // 触发 poll
                    bound.inner().iface().poll();
                    return Ok((*size, *src_addr));
                }
                result
            }
            Some(RawInner::Wildcard(bound)) => {
                let result = bound.try_recv(buf, self.ip_version);
                if let Ok((_size, _src_addr)) = &result {
                    bound.inner().iface().poll();
                }
                result
            }
            Some(RawInner::Unbound(_)) => Err(SystemError::ENOTCONN),
        }
    }

    /// Linux 语义：IPv4 raw socket 的 recv/recvfrom 返回整个 IPv4 包（含 IPv4 头）；
    /// IPv6 raw socket 默认只返回 payload（不含 IPv6 固定头）。
    pub(super) fn try_recv_user(
        &self,
        user_buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpAddress), SystemError> {
        match self.ip_version {
            IpVersion::Ipv4 => self.try_recv(user_buf),
            IpVersion::Ipv6 => {
                // 需要先接收完整 IPv6 包，用于解析 src addr / cmsg，随后只向用户拷贝 payload。
                let mut tmp = vec![
                    0u8;
                    user_buf
                        .len()
                        .saturating_add(IPV6_HEADER_LEN)
                        .max(IPV6_HEADER_LEN)
                ];
                let (recv_size, src_addr) = self.try_recv(&mut tmp)?;
                let start = IPV6_HEADER_LEN.min(recv_size);
                let payload = &tmp[start..recv_size];
                let to_copy = core::cmp::min(payload.len(), user_buf.len());
                user_buf[..to_copy].copy_from_slice(&payload[..to_copy]);
                Ok((to_copy, src_addr))
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
    ) -> Result<(), SystemError> {
        if msg.msg_name.is_null() || msg.msg_namelen == 0 {
            return Ok(());
        }

        let port_be = 0u16.to_be();

        match (self.ip_version, src_addr) {
            (IpVersion::Ipv4, IpAddress::Ipv4(v4)) => {
                let sa = SockAddrIn {
                    sin_family: crate::net::socket::AddressFamily::INet as u16,
                    sin_port: port_be,
                    sin_addr: v4.to_bits().to_be(),
                    sin_zero: [0; 8],
                };
                let want = core::mem::size_of::<SockAddrIn>().min(msg.msg_namelen as usize);
                let mut w = UserBufferWriter::new(msg.msg_name as *mut u8, want, true)?;
                let bytes = unsafe {
                    core::slice::from_raw_parts((&sa as *const SockAddrIn) as *const u8, want)
                };
                w.buffer_protected(0)?.write_to_user(0, bytes)?;
                msg.msg_namelen = want as u32;
            }
            (IpVersion::Ipv6, IpAddress::Ipv6(v6)) => {
                let sa = SockAddrIn6 {
                    sin6_family: crate::net::socket::AddressFamily::INet6 as u16,
                    sin6_port: port_be,
                    sin6_flowinfo: 0,
                    sin6_addr: v6.octets(),
                    sin6_scope_id: 0,
                };
                let want = core::mem::size_of::<SockAddrIn6>().min(msg.msg_namelen as usize);
                let mut w = UserBufferWriter::new(msg.msg_name as *mut u8, want, true)?;
                let bytes = unsafe {
                    core::slice::from_raw_parts((&sa as *const SockAddrIn6) as *const u8, want)
                };
                w.buffer_protected(0)?.write_to_user(0, bytes)?;
                msg.msg_namelen = want as u32;
            }
            _ => {
                msg.msg_namelen = 0;
            }
        }
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
    ) -> Result<(), SystemError> {
        if recv_size < IPV4_MIN_HEADER_LEN {
            return Ok(());
        }

        let tos = packet[1];
        let ttl = packet[8];
        let dst = u32::from_be_bytes([packet[16], packet[17], packet[18], packet[19]]);

        // IP_PKTINFO -> in_pktinfo
        if options.recv_pktinfo_v4 {
            let ifindex = self
                .inner
                .read()
                .as_ref()
                .and_then(|inner| match inner {
                    RawInner::Bound(b) | RawInner::Wildcard(b) => {
                        Some(b.inner().iface().nic_id() as i32)
                    }
                    _ => None,
                })
                .unwrap_or(0);
            let pktinfo = InPktInfo {
                ipi_ifindex: ifindex,
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
    ) -> Result<(), SystemError> {
        if recv_size < IPV6_HEADER_LEN {
            return Ok(());
        }

        let traffic_class = ((packet[0] & 0x0f) << 4) | (packet[1] >> 4);
        let hop_limit = packet[7];
        let dst = &packet[24..40];

        // IPV6_RECVPKTINFO -> in6_pktinfo
        if options.recv_pktinfo_v6 {
            let ifindex = self
                .inner
                .read()
                .as_ref()
                .and_then(|inner| match inner {
                    RawInner::Bound(b) | RawInner::Wildcard(b) => {
                        Some(b.inner().iface().nic_id() as u32)
                    }
                    _ => None,
                })
                .unwrap_or(0);
            let mut pktinfo = In6PktInfo::default();
            pktinfo.ipi6_addr.copy_from_slice(dst);
            pktinfo.ipi6_ifindex = ifindex;
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
            )?,
            IpVersion::Ipv6 => self.build_ipv6_cmsgs(
                &mut cmsg_buf,
                &mut msg.msg_flags,
                packet,
                recv_size,
                &options,
            )?,
        }

        Ok(write_off)
    }

    pub fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };

        // Linux 语义：IPv6 raw socket 接收时默认不向用户返回 IPv6 头；
        // 但控制消息(cmsg)需要从 IPv6 头提取，因此这里总是预留 IPv6 头空间。
        let user_len = iovs.total_len();
        let (need_head, extra_for_payload) = match self.ip_version {
            IpVersion::Ipv4 => (IPV4_MIN_HEADER_LEN, 0usize),
            IpVersion::Ipv6 => (IPV6_HEADER_LEN, IPV6_HEADER_LEN),
        };
        let mut tmp = vec![0u8; user_len.saturating_add(extra_for_payload).max(need_head)];

        let nonblock = self.is_nonblock() || flags.contains(crate::net::socket::PMSG::DONTWAIT);

        let (recv_size, src_addr) = if nonblock {
            self.try_recv(&mut tmp)
        } else {
            loop {
                match self.try_recv(&mut tmp) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    other => break other,
                }
            }
        }?;

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
        iovs.scatter(&user_data[..user_recv_size])?;

        // 默认不设置任何 flags。
        msg.msg_flags = 0;

        // 填充 peer 地址
        self.fill_peer_addr(msg, src_addr)?;

        // 构建控制消息
        msg.msg_controllen = self.build_recv_cmsgs(msg, &tmp, recv_size)?;

        Ok(user_recv_size)
    }

    pub fn validate_sendto_addr(
        &self,
        addr: *const crate::net::posix::SockAddr,
        addrlen: u32,
    ) -> Result<(), SystemError> {
        // Linux 语义：对 AF_INET6 socket，若用户提供目标地址但 addrlen 小于 sockaddr_in6，返回 EINVAL。
        // gVisor raw_socket_test: RawSocketTest.IPv6SendMsg
        if !addr.is_null()
            && self.is_ipv6()
            && (addrlen as usize) < core::mem::size_of::<crate::net::posix::SockAddrIn6>()
        {
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
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
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
        let port = 0u16;
        if self.is_nonblock() || flags.contains(crate::net::socket::PMSG::DONTWAIT) {
            self.try_recv_user(buffer).map(|(len, addr)| {
                (
                    len,
                    crate::net::socket::endpoint::Endpoint::Ip(smoltcp::wire::IpEndpoint::new(
                        addr, port,
                    )),
                )
            })
        } else {
            loop {
                match self.try_recv_user(buffer) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    result => {
                        return result.map(|(len, addr)| {
                            (
                                len,
                                crate::net::socket::endpoint::Endpoint::Ip(
                                    smoltcp::wire::IpEndpoint::new(addr, port),
                                ),
                            )
                        })
                    }
                }
            }
        }
    }
}
