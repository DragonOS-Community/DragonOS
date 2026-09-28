use core::sync::atomic::Ordering;
use smoltcp::wire::{IpAddress, IpProtocol, IpVersion, Ipv4Packet};
use system_error::SystemError;

use crate::driver::net::Iface;
use crate::filesystem::vfs::iov::IoVecs;
use crate::net::posix::SockAddr;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::unix::utils::{cmsg_align, Cmsghdr};
use crate::net::socket::utils::IPV4_MIN_HEADER_LEN;
use crate::net::socket::{IpOption, PIPV6, PMSG, PSOL};
use crate::syscall::user_access::UserBufferReader;

use super::inner::RawInner;
use super::loopback::is_loopback_addr;
use super::packet::{build_ip_packet, emit_ipv4_packet, IpPacketParams};
use super::RawSocket;

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

/// Keep SO_SNDTIMEO scoped to one send call, even if another writer takes the
/// queue space after this waiter is awakened.
fn remaining_send_timeout(
    started: crate::time::Instant,
    timeout: Option<crate::time::Duration>,
) -> Result<Option<crate::time::Duration>, SystemError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    let elapsed = crate::time::Instant::now().saturating_sub(started);
    let remaining = timeout
        .total_micros()
        .saturating_sub(elapsed.total_micros());
    if remaining == 0 {
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }
    Ok(Some(crate::time::Duration::from_micros(remaining)))
}

impl RawSocket {
    /// Build one complete IPv6 raw datagram before the synchronous OUTPUT
    /// verdict. HDRINCL keeps the caller's header verbatim; its sockaddr
    /// destination is still the initial route key, as in rawv6_send_hdrinc.
    fn try_send_ipv6_prepared(
        &self,
        buf: &[u8],
        to: Option<IpAddress>,
        options: &super::options::RawSocketOptions,
    ) -> Result<usize, SystemError> {
        if self.protocol == IpProtocol::Unknown(255) {
            return Err(SystemError::EINVAL);
        }
        let destination = to
            .or_else(|| match self.inner.read().as_ref() {
                Some(RawInner::Bound(bound) | RawInner::Wildcard(bound)) => bound.remote_addr(),
                _ => None,
            })
            .ok_or(SystemError::EDESTADDRREQ)?;
        if !matches!(destination, IpAddress::Ipv6(_)) {
            return Err(SystemError::EAFNOSUPPORT);
        }
        self.ensure_not_loopback_wildcard_for_send(destination)?;
        if !self.is_bound() {
            self.bind_ephemeral(destination)?;
        }
        let fixed_source = match self.inner.read().as_ref() {
            Some(RawInner::Bound(bound) | RawInner::Wildcard(bound)) => bound
                .local_addr()
                .filter(|address| !address.is_unspecified()),
            _ => return Err(SystemError::ENOTCONN),
        };
        let required_oif = self
            .device_binding
            .resolve_iface(&self.netns)?
            .map(|iface| iface.nic_id() as u32);
        let (route, owner, source) = if options.ip_hdrincl {
            let route = crate::net::route::resolve_ipv6_output_route(
                &self.netns,
                destination,
                required_oif,
                None,
            )?;
            let owner = self
                .netns
                .device_list()
                .get(&(route.oif as usize))
                .cloned()
                .ok_or(SystemError::ENETUNREACH)?;
            (route, owner, None)
        } else {
            let resolved = crate::net::route::resolve_ipv6_send_route(
                &self.netns,
                destination,
                required_oif,
                fixed_source,
            )?;
            (
                resolved.decision,
                resolved.source_owner,
                Some(resolved.source),
            )
        };
        if options.ip_hdrincl {
            if buf.len() < crate::net::socket::utils::IPV6_HEADER_LEN {
                return Err(SystemError::EINVAL);
            }
            if buf.len() > route.ip_mtu {
                return Err(SystemError::EMSGSIZE);
            }
        }
        let built;
        let packet = if options.ip_hdrincl {
            buf
        } else {
            let params = IpPacketParams {
                payload: buf,
                src: source.expect("non-HDRINCL output selected a source").into(),
                dst: destination,
                protocol: self.protocol,
                ttl: options.ip_ttl,
                tos: options.ip_tos,
                ipv6_checksum: options.ipv6_checksum,
            };
            built = build_ip_packet(IpVersion::Ipv6, &params)?;
            built.as_slice()
        };
        let mut reservation = crate::driver::net::local_output::reserve_prepared_ip_output(
            owner.as_ref(),
            packet.len(),
            IpVersion::Ipv6,
        )?;
        reservation.bytes_mut().copy_from_slice(packet);
        crate::net::output::submit_prepared_ipv6(&self.netns, reservation, route)?;
        Ok(buf.len())
    }

    /// One IPv4 send boundary for send, sendto, and sendmsg. The receive-side
    /// smoltcp attachment is only consulted for the explicit local bind and
    /// connected destination; it is never treated as the output route.
    fn try_send_ipv4_prepared(
        &self,
        buf: &[u8],
        to: Option<IpAddress>,
        options: &super::options::RawSocketOptions,
        ttl_override: Option<u8>,
    ) -> Result<usize, SystemError> {
        let multicast_loop = self.ip_multicast_loop.load(Ordering::Acquire);
        let multicast_ttl = self.ip_multicast_ttl.load(Ordering::Acquire) as u8;
        let (destination, bound_source) = {
            let inner = self.inner.read();
            let bound = match inner.as_ref() {
                Some(RawInner::Bound(bound) | RawInner::Wildcard(bound)) => Some(bound),
                Some(RawInner::Unbound(_)) => None,
                None => return Err(SystemError::EBADF),
            };
            let destination = to
                .or_else(|| bound.and_then(|bound| bound.remote_addr()))
                .ok_or(SystemError::EDESTADDRREQ)?;
            let bound_source = bound.and_then(|bound| bound.local_addr());
            (destination, bound_source)
        };
        if !matches!(destination, IpAddress::Ipv4(_)) {
            return Err(SystemError::EAFNOSUPPORT);
        }

        let mut required_oif = self
            .device_binding
            .resolve_iface(&self.netns)?
            .map(|iface| iface.nic_id() as u32);
        let mut fixed_source = bound_source.filter(|source| {
            !source.is_unspecified()
                && !source.is_multicast()
                && !crate::net::address::netns_accepts_broadcast_address(&self.netns, *source)
        });
        if destination.is_multicast() {
            let multicast_oif = self.ip_multicast_ifindex.load(Ordering::Acquire);
            let multicast_addr = self.ip_multicast_addr.load(Ordering::Acquire);
            required_oif = required_oif.or_else(|| {
                u32::try_from(multicast_oif)
                    .ok()
                    .filter(|index| *index != 0)
            });
            if fixed_source.is_none() && multicast_addr != 0 {
                let octets = multicast_addr.to_ne_bytes();
                fixed_source = Some(IpAddress::v4(octets[0], octets[1], octets[2], octets[3]));
            }
        }
        let resolved = crate::net::route::resolve_ipv4_route(
            &self.netns,
            destination,
            required_oif,
            fixed_source,
        )?;
        let route = resolved.output_decision();
        if route.kind == crate::net::route::RTN_BROADCAST
            && !self.so_broadcast.load(Ordering::Acquire)
        {
            return Err(SystemError::EACCES);
        }

        let packet_len = if options.ip_hdrincl {
            validate_ipv4_hdrincl_packet(buf)?;
            buf.len()
        } else {
            buf.len()
                .checked_add(IPV4_MIN_HEADER_LEN)
                .filter(|len| *len <= u16::MAX as usize)
                .ok_or(SystemError::EMSGSIZE)?
        };
        // Linux raw_send_hdrinc rejects an oversized complete datagram even
        // when its supplied header has DF clear.
        if options.ip_hdrincl && packet_len > route.ip_mtu {
            return Err(SystemError::EMSGSIZE);
        }
        let owner = self
            .netns
            .device_list()
            .get(&(route.oif as usize))
            .cloned()
            .ok_or(SystemError::ENETUNREACH)?;
        let charge = self.send_account.charge(packet_len)?;
        let mut reservation = crate::driver::net::local_output::reserve_prepared_ip_output(
            owner.as_ref(),
            packet_len,
            smoltcp::wire::IpVersion::Ipv4,
        )?;

        if options.ip_hdrincl {
            let packet = reservation.bytes_mut();
            packet.copy_from_slice(buf);
            let total_len = packet_len as u16;
            let mut ip = Ipv4Packet::new_unchecked(packet);
            ip.set_total_len(total_len);
            if ip.src_addr().is_unspecified() {
                let IpAddress::Ipv4(source) = resolved.source else {
                    return Err(SystemError::EAFNOSUPPORT);
                };
                ip.set_src_addr(source);
            }
            if ip.ident() == 0 {
                ip.set_ident(self.netns.next_ipv4_identification());
            }
            ip.fill_checksum();
        } else {
            let ttl = ttl_override.unwrap_or(if destination.is_multicast() {
                multicast_ttl
            } else {
                options.ip_ttl
            });
            let dont_fragment = packet_len <= route.ip_mtu;
            let ident = if dont_fragment {
                0
            } else {
                self.netns.next_ipv4_identification()
            };
            emit_ipv4_packet(
                reservation.bytes_mut(),
                &IpPacketParams {
                    payload: buf,
                    src: resolved.source,
                    dst: destination,
                    protocol: self.protocol,
                    ttl,
                    tos: options.ip_tos,
                    ipv6_checksum: options.ipv6_checksum,
                },
                ident,
                dont_fragment,
            )?;
        }
        reservation.set_charge(charge);
        let _ = crate::net::output::submit_prepared_ipv4(
            &self.netns,
            reservation,
            route,
            multicast_loop,
            !options.ip_hdrincl,
        )?;
        Ok(buf.len())
    }

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

        if self.ip_version == IpVersion::Ipv4 {
            let options = self.options.read().clone();
            return self.try_send_ipv4_prepared(buf, to, &options, None);
        }

        self.try_send_ipv6_prepared(buf, to, &self.options.read().clone())
    }

    pub fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
            return self.try_send(buffer, None);
        }

        let started = crate::time::Instant::now();
        let timeout = self.send_timeout();

        loop {
            match self.try_send(buffer, None) {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) if self.ip_version == IpVersion::Ipv4 => {
                    let packet_len =
                        buffer
                            .len()
                            .saturating_add(if self.options.read().ip_hdrincl {
                                0
                            } else {
                                IPV4_MIN_HEADER_LEN
                            });
                    self.wait_queue.wait_event_io_interruptible_timeout(
                        || self.send_account.can_charge(packet_len),
                        remaining_send_timeout(started, timeout)?,
                    )?;
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

            let started = crate::time::Instant::now();
            let timeout = self.send_timeout();

            loop {
                match self.try_send(buffer, Some(remote.addr)) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                        if self.ip_version == IpVersion::Ipv4 =>
                    {
                        let packet_len =
                            buffer
                                .len()
                                .saturating_add(if self.options.read().ip_hdrincl {
                                    0
                                } else {
                                    IPV4_MIN_HEADER_LEN
                                });
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || self.send_account.can_charge(packet_len),
                            remaining_send_timeout(started, timeout)?,
                        )?;
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
        flags: PMSG,
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
        let mut ipv4_ttl_override = None;

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
                    (level, t) if level == PSOL::IP as i32 && t == IpOption::TTL as i32 => {
                        if let Some(v) = read_i32(data) {
                            let ttl = v.clamp(0, 255) as u8;
                            options.ip_ttl = ttl;
                            ipv4_ttl_override = Some(ttl);
                        }
                    }
                    (level, t) if level == PSOL::IP as i32 && t == IpOption::TOS as i32 => {
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

        if self.ip_version == IpVersion::Ipv4 {
            if !self.addr_matches_ip_version(dest) {
                return Err(SystemError::EAFNOSUPPORT);
            }
            if flags.contains(PMSG::DONTWAIT) || self.is_nonblock() {
                return self.try_send_ipv4_prepared(&buf, Some(dest), &options, ipv4_ttl_override);
            }
            let started = crate::time::Instant::now();
            let timeout = self.send_timeout();
            loop {
                match self.try_send_ipv4_prepared(&buf, Some(dest), &options, ipv4_ttl_override) {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || {
                                self.send_account.can_charge(buf.len().saturating_add(
                                    if options.ip_hdrincl {
                                        0
                                    } else {
                                        IPV4_MIN_HEADER_LEN
                                    },
                                ))
                            },
                            remaining_send_timeout(started, timeout)?,
                        )?;
                    }
                    result => return result,
                }
            }
        }

        self.try_send_ipv6_prepared(&buf, Some(dest), &options)
    }
}
