use smoltcp::wire::{IpAddress, IpProtocol, IpVersion, Ipv4Packet};
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::filesystem::vfs::iov::IoVecs;
use crate::net::posix::SockAddr;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::unix::utils::{cmsg_align, Cmsghdr};
use crate::net::socket::utils::IPV4_MIN_HEADER_LEN;
use crate::net::socket::{PIP, PIPV6, PMSG, PSOL};
use crate::syscall::user_access::UserBufferReader;

use super::inner::{self, RawInner};
use super::loopback::{deliver_loopback_packet, is_loopback_addr, LoopbackDeliverContext};
use super::packet::{build_ip_packet, IpPacketParams};
use super::RawSocket;

struct HdrinclSendOutcome {
    bytes_written: usize,
    needs_iface_poll: bool,
}

fn validate_ipv4_hdrincl_packet(buf: &[u8]) -> Result<(), SystemError> {
    if buf.len() < IPV4_MIN_HEADER_LEN {
        return Err(SystemError::EINVAL);
    }

    let ihl = ((buf[0] & 0x0f) as usize) * 4;
    // Linux raw_send_hdrinc: ihl must be sane and not exceed the provided buffer.
    if ihl < IPV4_MIN_HEADER_LEN || ihl > buf.len() {
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

impl RawSocket {
    /// 发送前确保 socket 绑定在合适的 iface 上。
    ///
    /// 背景：raw socket 在创建时可能处于 Wildcard 状态并附着到 loopback 以便接收/唤醒。
    /// 但对非 loopback 目的地址发送时，Linux 语义应根据目的地址选路/选出口网卡，
    /// 而不是把发送也锁死在 loopback。
    fn ensure_not_loopback_wildcard_for_send(&self, dest: IpAddress) -> Result<(), SystemError> {
        // loopback 目的地址仍走 loopback 快速路径，不需要切换 iface
        if is_loopback_addr(dest) {
            return Ok(());
        }

        let needs_rebind = {
            let guard = self.inner.read();
            match guard.as_ref() {
                Some(RawInner::Wildcard(bound)) => {
                    if let Some(lo) = self.netns.loopback_iface() {
                        bound.inner().iface().nic_id() == lo.nic_id()
                    } else {
                        false
                    }
                }
                _ => false,
            }
        };

        if needs_rebind {
            // 从 Wildcard(lo) 切换为按目的地址选址的 Bound(iface)。
            self.bind_ephemeral(dest)?;
        }
        Ok(())
    }

    fn send_ipv4_hdrincl_on_bound(
        &self,
        bound: &inner::BoundRaw,
        buf: &[u8],
        dest: IpAddress,
    ) -> Result<HdrinclSendOutcome, SystemError> {
        validate_ipv4_hdrincl_packet(buf)?;

        let pkt_proto = IpProtocol::from(buf[9]);

        // Linux raw_send_hdrinc: if iph->saddr == 0, stack sets it.
        let src_addr = self.get_src_addr_for_send(bound, dest)?;
        let src_v4 = match src_addr {
            IpAddress::Ipv4(v4) => v4,
            _ => return Err(SystemError::EAFNOSUPPORT),
        };

        // Patch the IPv4 header similarly to Linux: tot_len is overwritten, checksum recomputed.
        let mut packet = buf.to_vec();
        if packet.len() <= u16::MAX as usize {
            let total_len = packet.len() as u16;
            let existing_saddr_is_zero = packet[12..16].iter().all(|b| *b == 0);

            let mut pkt = Ipv4Packet::new_unchecked(&mut packet);
            pkt.set_total_len(total_len);
            if existing_saddr_is_zero {
                pkt.set_src_addr(src_v4);
            }
            pkt.fill_checksum();
        }

        // loopback 快速路径：路由/投递目的由 sendto/connect 指定的 dest 决定，
        // 而不是 IP header 里的 daddr（gVisor RawHDRINCL.SendAndReceiveDifferentAddress）。
        if is_loopback_addr(dest) {
            let ctx = LoopbackDeliverContext {
                packet: &packet,
                dest,
                ip_version: self.ip_version,
                protocol: pkt_proto,
                netns: &self.netns,
            };
            deliver_loopback_packet(&ctx);
            return Ok(HdrinclSendOutcome {
                bytes_written: packet.len(),
                needs_iface_poll: false,
            });
        }

        bound.try_send(&packet, Some(dest))?;
        Ok(HdrinclSendOutcome {
            bytes_written: packet.len(),
            needs_iface_poll: true,
        })
    }

    /// 尝试发送数据包
    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpAddress>,
    ) -> Result<usize, SystemError> {
        // Linux 语义：AF_INET6/SOCK_RAW/IPPROTO_RAW 可以创建，但写入返回 EINVAL。
        // gVisor raw_socket_test: RawSocketTest.IPv6ProtoRaw
        if self.is_ipv6() && self.protocol == IpProtocol::Unknown(255) {
            return Err(SystemError::EINVAL);
        }

        if let Some(dest) = to {
            if !self.addr_matches_ip_version(dest) {
                return Err(SystemError::EAFNOSUPPORT);
            }
        }

        // 若当前处于 Wildcard(loopback)，对非 loopback 目的地址发送时需要切到正确出口。
        if let Some(dest) = to {
            self.ensure_not_loopback_wildcard_for_send(dest)?;
        }

        // 确保已绑定
        if !self.is_bound() {
            if let Some(dest) = to {
                self.bind_ephemeral(dest)?;
            } else {
                return Err(SystemError::EDESTADDRREQ);
            }
        }

        let inner_guard = self.inner.read();
        match inner_guard.as_ref() {
            None => Err(SystemError::ENOTCONN),
            Some(RawInner::Bound(bound)) => {
                let sent = self.try_send_on_bound(bound, buf, to)?;
                bound.inner().iface().poll();
                Ok(sent)
            }
            Some(RawInner::Wildcard(bound)) => {
                let sent = self.try_send_on_bound(bound, buf, to)?;
                bound.inner().iface().poll();
                Ok(sent)
            }
            Some(RawInner::Unbound(_)) => Err(SystemError::ENOTCONN),
        }
    }

    fn try_send_on_bound(
        &self,
        bound: &inner::BoundRaw,
        buf: &[u8],
        to: Option<IpAddress>,
    ) -> Result<usize, SystemError> {
        let options = self.options.read().clone();

        // 目标地址：sendto() 显式指定优先；否则使用 connect(2) 的远端。
        let dest = to.or(bound.remote_addr());

        if options.ip_hdrincl {
            // Linux 语义：即便用户在 IP header 里提供了 daddr，send(2) 在未 connect
            // 且未提供 msg_name/sockaddr 的情况下仍返回 EDESTADDRREQ。
            let dest = dest.ok_or(SystemError::EDESTADDRREQ)?;

            match self.ip_version {
                IpVersion::Ipv4 => {
                    let out = self.send_ipv4_hdrincl_on_bound(bound, buf, dest)?;
                    return Ok(out.bytes_written);
                }
                IpVersion::Ipv6 => return Err(SystemError::EINVAL),
            }
        }

        // 用户未提供 IP 头：按 Linux 语义内核自动构造。
        // gVisor raw_socket_test: RawSocketTest.ReceiveIPPacketInfo 等
        let dest = dest.ok_or(SystemError::EDESTADDRREQ)?;

        // 获取源地址
        let src = self.get_src_addr_for_send(bound, dest)?;

        let params = IpPacketParams {
            payload: buf,
            src,
            dst: dest,
            protocol: self.protocol,
            ttl: options.ip_ttl,
            tos: options.ip_tos,
            ipv6_checksum: options.ipv6_checksum,
        };

        let packet = build_ip_packet(self.ip_version, &params)?;

        // loopback 快速路径：避免 smoltcp 重序列化导致 TOS/TCLASS 丢失，
        // 并在此处实现 SO_RCVBUF 的投递/丢弃语义。
        if is_loopback_addr(dest) {
            let ctx = LoopbackDeliverContext {
                packet: &packet,
                dest,
                ip_version: self.ip_version,
                protocol: self.protocol,
                netns: &self.netns,
            };
            deliver_loopback_packet(&ctx);
            // Linux/Netstack：即便因 rcvbuf 满或过滤丢包，sendmsg/sendto 仍可成功。
            return Ok(buf.len());
        }

        bound.try_send(&packet, Some(dest))?;
        Ok(buf.len())
    }

    /// 获取发送时使用的源地址
    fn get_src_addr_for_send(
        &self,
        bound: &inner::BoundRaw,
        _dest: IpAddress,
    ) -> Result<IpAddress, SystemError> {
        match self.ip_version {
            IpVersion::Ipv4 => match bound.local_addr() {
                Some(addr @ IpAddress::Ipv4(_)) => Ok(addr),
                _ => {
                    let ip = bound
                        .inner()
                        .iface()
                        .common()
                        .ipv4_addr()
                        .ok_or(SystemError::EADDRNOTAVAIL)?;
                    let [a, b, c, d] = ip.octets();
                    Ok(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(a, b, c, d)))
                }
            },
            IpVersion::Ipv6 => match bound.local_addr() {
                Some(addr @ IpAddress::Ipv6(_)) => Ok(addr),
                _ => {
                    let iface = bound.inner().iface();
                    let addr = iface
                        .smol_iface()
                        .lock()
                        .ipv6_addr()
                        .ok_or(SystemError::EADDRNOTAVAIL)?;
                    Ok(IpAddress::Ipv6(addr))
                }
            },
        }
    }

    pub fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
            return self.try_send(buffer, None);
        }

        loop {
            match self.try_send(buffer, None) {
                Err(SystemError::ENOBUFS) => {
                    wq_wait_event_interruptible!(self.wait_queue, self.can_send(), {})?;
                }
                result => return result,
            }
        }
    }

    pub fn send_to(
        &self,
        buffer: &[u8],
        flags: PMSG,
        address: Endpoint,
    ) -> Result<usize, SystemError> {
        if let Endpoint::Ip(remote) = address {
            if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
                return self.try_send(buffer, Some(remote.addr));
            }

            loop {
                match self.try_send(buffer, Some(remote.addr)) {
                    Err(SystemError::ENOBUFS) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_send(), {})?;
                    }
                    result => return result,
                }
            }
        }
        Err(SystemError::EINVAL)
    }

    pub fn send_msg(
        &self,
        msg: &crate::net::posix::MsgHdr,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        // Gather payload.
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let buf = iovs.gather()?;

        // Parse destination address if provided.
        let mut to_ip: Option<IpAddress> = if msg.msg_name.is_null() {
            None
        } else {
            let ep = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            match ep {
                Endpoint::Ip(ip) => Some(ip.addr),
                _ => return Err(SystemError::EAFNOSUPPORT),
            }
        };

        // Clone current options and apply per-send overrides from cmsgs.
        let mut options = self.options.read().clone();

        if !msg.msg_control.is_null() && msg.msg_controllen != 0 {
            let reader =
                UserBufferReader::new(msg.msg_control as *const u8, msg.msg_controllen, true)?;
            let mut cbuf = vec![0u8; msg.msg_controllen];
            reader.copy_from_user(&mut cbuf, 0)?;

            let hdr_len = core::mem::size_of::<Cmsghdr>();
            let mut off = 0usize;

            let read_i32 = |d: &[u8]| -> Option<i32> {
                if d.len() >= 4 {
                    Some(i32::from_ne_bytes([d[0], d[1], d[2], d[3]]))
                } else {
                    None
                }
            };

            while off + hdr_len <= cbuf.len() {
                let hdr: Cmsghdr =
                    unsafe { core::ptr::read_unaligned(cbuf.as_ptr().add(off) as *const Cmsghdr) };
                if hdr.cmsg_len < hdr_len {
                    break;
                }

                let cmsg_len = core::cmp::min(hdr.cmsg_len, cbuf.len() - off);
                let data_off = off + cmsg_align(hdr_len);
                let data_len = cmsg_len.saturating_sub(cmsg_align(hdr_len));
                let data = if data_off <= cbuf.len() {
                    let end = core::cmp::min(data_off + data_len, cbuf.len());
                    &cbuf[data_off..end]
                } else {
                    &[]
                };

                match (hdr.cmsg_level, hdr.cmsg_type) {
                    (level, t) if level == PSOL::IP as i32 && t == PIP::TTL as i32 => {
                        if let Some(v) = read_i32(data) {
                            options.ip_ttl = v.clamp(0, 255) as u8;
                        }
                    }
                    (level, t) if level == PSOL::IP as i32 && t == PIP::TOS as i32 => {
                        // gVisor 的 SendTOS 使用 uint8_t 作为 cmsg value。
                        if let Some(&v) = data.first() {
                            options.ip_tos = v;
                        }
                    }
                    (level, t) if level == PSOL::IPV6 as i32 && t == PIPV6::HOPLIMIT as i32 => {
                        if let Some(v) = read_i32(data) {
                            options.ip_ttl = v.clamp(0, 255) as u8;
                        }
                    }
                    (level, t) if level == PSOL::IPV6 as i32 && t == PIPV6::TCLASS as i32 => {
                        if let Some(v) = read_i32(data) {
                            options.ip_tos = v.clamp(0, 255) as u8;
                        }
                    }
                    _ => {}
                }

                let step = cmsg_align(cmsg_len);
                if step == 0 {
                    break;
                }
                off = off.saturating_add(step);
            }
        }

        // Resolve destination from connect(2) if not explicitly provided.
        if to_ip.is_none() {
            if let Some(RawInner::Bound(b) | RawInner::Wildcard(b)) = self.inner.read().as_ref() {
                to_ip = b.remote_addr();
            }
        }
        let dest = to_ip.ok_or(SystemError::EDESTADDRREQ)?;

        // 若当前处于 Wildcard(loopback)，对非 loopback 目的地址发送时需要切到正确出口。
        self.ensure_not_loopback_wildcard_for_send(dest)?;

        // Ensure bound.
        if !self.is_bound() {
            self.bind_ephemeral(dest)?;
        }

        let inner_guard = self.inner.read();
        let bound = match inner_guard.as_ref() {
            Some(RawInner::Bound(b)) => b,
            Some(RawInner::Wildcard(b)) => b,
            _ => return Err(SystemError::ENOTCONN),
        };

        if options.ip_hdrincl {
            match self.ip_version {
                IpVersion::Ipv4 => {
                    let out = self.send_ipv4_hdrincl_on_bound(bound, &buf, dest)?;
                    if out.needs_iface_poll {
                        bound.inner().iface().poll();
                    }
                    return Ok(out.bytes_written);
                }
                IpVersion::Ipv6 => return Err(SystemError::EINVAL),
            }
        }

        // 获取源地址
        let src = self.get_src_addr_for_send(bound, dest)?;

        let params = IpPacketParams {
            payload: &buf,
            src,
            dst: dest,
            protocol: self.protocol,
            ttl: options.ip_ttl,
            tos: options.ip_tos,
            ipv6_checksum: options.ipv6_checksum,
        };

        let packet = build_ip_packet(self.ip_version, &params)?;

        // loopback 快速路径
        if is_loopback_addr(dest) {
            let ctx = LoopbackDeliverContext {
                packet: &packet,
                dest,
                ip_version: self.ip_version,
                protocol: self.protocol,
                netns: &self.netns,
            };
            deliver_loopback_packet(&ctx);
            // Linux/Netstack：即便因 rcvbuf 满或过滤丢包，sendmsg 仍可成功。
            return Ok(buf.len());
        }

        bound.try_send(&packet, Some(dest))?;
        bound.inner().iface().poll();
        Ok(buf.len())
    }
}
