//! UDP socket option handling.
//!
//! 本模块集中实现 `UdpSocket` 的 SOL_SOCKET 级别 option 处理逻辑，
//! 参考 TCP 的 `inet/stream/option.rs` 进行拆分。

use core::sync::atomic::Ordering;

use system_error::SystemError;

use super::inner::{DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE};
use super::UdpSocket;
use crate::libs::byte_parser;
use crate::net::socket::common::{parse_timeval_opt, write_timeval_opt};
use crate::net::socket::inet::common::{apply_ipv4_membership, apply_ipv4_multicast_if};
use crate::net::socket::{AddressFamily, IpOption, PIPV6, PSO, PSOCK, PSOL};
use crate::process::cred::CAPFlags;
use crate::process::ProcessManager;

use super::super::raw::{SOCK_MIN_RCVBUF, SOCK_MIN_SNDBUF, SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX};

// Returns the user-visible buffer size (so getsockopt returns size*2), while
// enforcing Linux-like sysctl max and minimums to avoid huge allocations.
fn clamp_udp_buf(val_u32: u32, sysctl_max: u32, sock_min: u32) -> usize {
    let mut val = core::cmp::min(val_u32, sysctl_max);
    val = core::cmp::min(val, (i32::MAX as u32) / 2);
    let doubled = core::cmp::max(val.saturating_mul(2), sock_min);
    doubled.div_ceil(2) as usize
}

// Force variant: no sysctl max clamp, but still prevents overflow and enforces minimum.
fn clamp_udp_buf_force(val_i32: i32, sock_min: u32) -> usize {
    let mut val = if val_i32 < 0 { 0 } else { val_i32 as u32 };
    val = core::cmp::min(val, (i32::MAX as u32) / 2);
    let doubled = core::cmp::max(val.saturating_mul(2), sock_min);
    doubled.div_ceil(2) as usize
}

impl UdpSocket {
    /// 处理 SOL_SOCKET 级别的 setsockopt。
    pub(super) fn set_socket_option(&self, opt: PSO, val: &[u8]) -> Result<(), SystemError> {
        match opt {
            PSO::SNDBUF => {
                let requested = byte_parser::read_u32(val)?;
                let size = clamp_udp_buf(requested, SYSCTL_WMEM_MAX, SOCK_MIN_SNDBUF);
                self.send_buf_size.store(size, Ordering::Release);

                // If socket is already bound, we need to recreate it with new buffer size
                self.recreate_socket_if_bound()?;
                Ok(())
            }
            PSO::RCVBUF => {
                let requested = byte_parser::read_u32(val)?;
                let size = clamp_udp_buf(requested, SYSCTL_RMEM_MAX, SOCK_MIN_RCVBUF);
                self.recv_buf_size.store(size, Ordering::Release);

                // If socket is already bound, we need to recreate it with new buffer size
                self.recreate_socket_if_bound()?;
                Ok(())
            }
            PSO::RCVLOWAT => {
                let mut v = byte_parser::read_i32(val)?;
                if v < 0 {
                    v = i32::MAX;
                } else if v == 0 {
                    v = 1;
                }
                self.rcvlowat.store(v, Ordering::Relaxed);
                Ok(())
            }
            PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW => {
                let d = parse_timeval_opt(val)?;
                let us = d.map(|v| v.total_micros()).unwrap_or(u64::MAX);
                self.send_timeout_us.store(us, Ordering::Relaxed);
                Ok(())
            }
            PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW => {
                let d = parse_timeval_opt(val)?;
                let us = d.map(|v| v.total_micros()).unwrap_or(u64::MAX);
                self.recv_timeout_us.store(us, Ordering::Relaxed);
                Ok(())
            }
            PSO::SNDBUFFORCE => {
                let cred = ProcessManager::current_pcb().cred();
                if !cred.has_capability(CAPFlags::CAP_NET_ADMIN) {
                    return Err(SystemError::EPERM);
                }
                let requested = byte_parser::read_i32(val)?;
                let size = clamp_udp_buf_force(requested, SOCK_MIN_SNDBUF);
                self.send_buf_size.store(size, Ordering::Release);
                self.recreate_socket_if_bound()?;
                Ok(())
            }
            PSO::RCVBUFFORCE => {
                let cred = ProcessManager::current_pcb().cred();
                if !cred.has_capability(CAPFlags::CAP_NET_ADMIN) {
                    return Err(SystemError::EPERM);
                }
                let requested = byte_parser::read_i32(val)?;
                let size = clamp_udp_buf_force(requested, SOCK_MIN_RCVBUF);
                self.recv_buf_size.store(size, Ordering::Release);
                self.recreate_socket_if_bound()?;
                Ok(())
            }
            PSO::REUSEADDR => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                self.so_reuseaddr.store(v != 0, Ordering::Relaxed);
                Ok(())
            }
            PSO::BROADCAST => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                self.so_broadcast.store(v != 0, Ordering::Relaxed);
                Ok(())
            }
            PSO::PASSCRED => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                self.so_passcred.store(v != 0, Ordering::Relaxed);
                Ok(())
            }
            PSO::REUSEPORT => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                self.so_reuseport.store(v != 0, Ordering::Relaxed);
                Ok(())
            }
            PSO::KEEPALIVE => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                self.so_keepalive.store(v != 0, Ordering::Relaxed);
                Ok(())
            }
            PSO::LINGER => {
                if val.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                let l_onoff = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                let l_linger = i32::from_ne_bytes([val[4], val[5], val[6], val[7]]);
                let on = if l_onoff != 0 { 1 } else { 0 };
                self.linger_onoff.store(on, Ordering::Relaxed);
                if on != 0 {
                    let v = if l_linger < 0 { i32::MAX } else { l_linger };
                    self.linger_linger.store(v, Ordering::Relaxed);
                }
                Ok(())
            }
            PSO::NO_CHECK => {
                // Set SO_NO_CHECK: disable/enable UDP checksum verification
                // NOTE: This is a stub implementation - see field comment for details.
                // The value is stored but does not affect actual checksum behavior.
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let value = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                self.no_check.store(value != 0, Ordering::Release);
                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    /// 处理 SOL_SOCKET 级别的 getsockopt。
    pub(super) fn get_socket_option(
        &self,
        opt: PSO,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match opt {
            PSO::TYPE => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = PSOCK::Datagram as i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::DOMAIN => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let domain = match self.ip_version {
                    smoltcp::wire::IpVersion::Ipv6 => AddressFamily::INet6,
                    smoltcp::wire::IpVersion::Ipv4 => AddressFamily::INet,
                };
                let v = domain as i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::PROTOCOL => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = PSOL::UDP as i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::SNDBUF => {
                if value.len() < core::mem::size_of::<u32>() {
                    return Err(SystemError::EINVAL);
                }
                let size = self.send_buf_size.load(Ordering::Acquire);
                // Linux doubles the value when returning it
                // If 0 (not set), return default size
                let actual_size = if size == 0 {
                    DEFAULT_TX_BUF_SIZE * 2
                } else {
                    size * 2
                };
                let bytes = (actual_size as u32).to_ne_bytes();
                value[0..4].copy_from_slice(&bytes);
                Ok(core::mem::size_of::<u32>())
            }
            PSO::RCVBUF => {
                if value.len() < core::mem::size_of::<u32>() {
                    return Err(SystemError::EINVAL);
                }
                let size = self.recv_buf_size.load(Ordering::Acquire);
                // Linux doubles the value when returning it
                // If 0 (not set), return default size
                let actual_size = if size == 0 {
                    DEFAULT_RX_BUF_SIZE * 2
                } else {
                    size * 2
                };
                let bytes = (actual_size as u32).to_ne_bytes();
                value[0..4].copy_from_slice(&bytes);
                Ok(core::mem::size_of::<u32>())
            }
            PSO::RCVLOWAT => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = self.rcvlowat.load(Ordering::Relaxed);
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW => {
                let us = self.send_timeout_us.load(Ordering::Relaxed);
                let us = if us == u64::MAX { 0 } else { us };
                write_timeval_opt(value, us)
            }
            PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW => {
                let us = self.recv_timeout_us.load(Ordering::Relaxed);
                let us = if us == u64::MAX { 0 } else { us };
                write_timeval_opt(value, us)
            }
            PSO::REUSEADDR => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = if self.so_reuseaddr.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::BROADCAST => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = if self.so_broadcast.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::PASSCRED => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = if self.so_passcred.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::REUSEPORT => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = if self.so_reuseport.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::KEEPALIVE => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = if self.so_keepalive.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::LINGER => {
                if value.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                let on = self.linger_onoff.load(Ordering::Relaxed);
                let linger = if on != 0 {
                    self.linger_linger.load(Ordering::Relaxed)
                } else {
                    0
                };
                value[0..4].copy_from_slice(&on.to_ne_bytes());
                value[4..8].copy_from_slice(&linger.to_ne_bytes());
                Ok(8)
            }
            PSO::ACCEPTCONN => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = 0i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::ERROR => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = 0i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(core::mem::size_of::<i32>())
            }
            PSO::NO_CHECK => {
                if value.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let no_check = self.no_check.load(Ordering::Acquire);
                let val = if no_check { 1i32 } else { 0i32 };
                let bytes = val.to_ne_bytes();
                value[0..4].copy_from_slice(&bytes);
                Ok(core::mem::size_of::<i32>())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    /// 处理 SOL_IP 级别的 setsockopt。
    pub(super) fn set_ip_option(&self, opt: IpOption, val: &[u8]) -> Result<(), SystemError> {
        match opt {
            IpOption::RECVTOS => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]) != 0;
                self.recv_tos.store(v, Ordering::Relaxed);
                Ok(())
            }
            IpOption::RECVERR | IpOption::RECVERR_RFC4884 => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]) != 0;
                self.recv_err_v4.store(v, Ordering::Relaxed);
                Ok(())
            }
            IpOption::MULTICAST_TTL => {
                let v = if val.len() == 1 {
                    val[0] as i32
                } else if val.len() >= core::mem::size_of::<i32>() {
                    i32::from_ne_bytes([val[0], val[1], val[2], val[3]])
                } else {
                    return Err(SystemError::EINVAL);
                };
                let ttl = if v == -1 {
                    1
                } else if (0..=255).contains(&v) {
                    v
                } else {
                    return Err(SystemError::EINVAL);
                };
                self.ip_multicast_ttl.store(ttl, Ordering::Relaxed);
                Ok(())
            }
            IpOption::MULTICAST_LOOP => {
                let v = if val.len() == 1 {
                    val[0] as i32
                } else if val.len() >= core::mem::size_of::<i32>() {
                    i32::from_ne_bytes([val[0], val[1], val[2], val[3]])
                } else {
                    return Err(SystemError::EINVAL);
                };
                let on = v != 0;
                self.ip_multicast_loop.store(on, Ordering::Relaxed);
                Ok(())
            }
            IpOption::MULTICAST_IF => apply_ipv4_multicast_if(
                &self.netns,
                val,
                &self.ip_multicast_ifindex,
                &self.ip_multicast_addr,
            ),
            IpOption::PKTINFO => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]) != 0;
                self.recv_pktinfo_v4.store(v, Ordering::Relaxed);
                Ok(())
            }
            IpOption::ORIGDSTADDR => {
                if val.len() < core::mem::size_of::<i32>() {
                    return Err(SystemError::EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]) != 0;
                self.recv_origdstaddr_v4.store(v, Ordering::Relaxed);
                Ok(())
            }
            IpOption::ADD_MEMBERSHIP | IpOption::DROP_MEMBERSHIP => {
                // First, apply the membership at the interface level
                apply_ipv4_membership(&self.netns, opt, val, &self.ip_multicast_groups)?;

                // Then, register/unregister with multicast loopback registry
                use super::multicast_loopback::multicast_registry;
                use crate::net::socket::inet::common::multicast::parse_mreqn_for_membership;

                if let Ok((multiaddr, ifaddr, ifindex)) = parse_mreqn_for_membership(val) {
                    // Determine the interface index
                    let resolved_ifindex = if ifindex != 0 {
                        ifindex
                    } else if ifaddr != 0 {
                        // Find interface by address
                        use crate::net::socket::inet::common::multicast::find_iface_by_ipv4;
                        find_iface_by_ipv4(&self.netns, ifaddr)
                            .map(|iface| iface.nic_id() as i32)
                            .unwrap_or(0)
                    } else {
                        // Use default interface
                        use crate::net::socket::inet::common::multicast::choose_default_ipv4_iface;
                        choose_default_ipv4_iface(&self.netns)
                            .map(|iface| iface.nic_id() as i32)
                            .unwrap_or(0)
                    };

                    if resolved_ifindex != 0 {
                        if opt == IpOption::ADD_MEMBERSHIP {
                            multicast_registry().register(
                                self.self_ref.clone(),
                                multiaddr,
                                resolved_ifindex,
                            );
                        } else {
                            multicast_registry().unregister(
                                &self.self_ref,
                                multiaddr,
                                resolved_ifindex,
                            );
                        }
                    }
                }

                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    /// 处理 SOL_IP 级别的 getsockopt。
    pub(super) fn get_ip_option(
        &self,
        opt: IpOption,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        if value.len() < core::mem::size_of::<i32>() {
            return Err(SystemError::EINVAL);
        }

        let v = match opt {
            IpOption::RECVTOS => {
                if self.recv_tos.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            IpOption::RECVERR | IpOption::RECVERR_RFC4884 => {
                if self.recv_err_v4.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            IpOption::MULTICAST_TTL => self.ip_multicast_ttl.load(Ordering::Relaxed),
            IpOption::MULTICAST_LOOP => {
                if self.ip_multicast_loop.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            IpOption::MULTICAST_IF => self.ip_multicast_addr.load(Ordering::Relaxed) as i32,
            IpOption::PKTINFO => {
                if self.recv_pktinfo_v4.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            IpOption::ORIGDSTADDR => {
                if self.recv_origdstaddr_v4.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            _ => return Err(SystemError::ENOPROTOOPT),
        };

        value[..4].copy_from_slice(&v.to_ne_bytes());
        Ok(core::mem::size_of::<i32>())
    }

    /// 处理 SOL_IPV6 级别的 setsockopt。
    pub(super) fn set_ipv6_option(&self, opt: PIPV6, val: &[u8]) -> Result<(), SystemError> {
        if val.len() < core::mem::size_of::<i32>() {
            return Err(SystemError::EINVAL);
        }
        let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]) != 0;

        match opt {
            PIPV6::RECVTCLASS => {
                self.recv_tclass.store(v, Ordering::Relaxed);
                Ok(())
            }
            PIPV6::RECVERR | PIPV6::RECVERR_RFC4884 => {
                self.recv_err_v6.store(v, Ordering::Relaxed);
                Ok(())
            }
            PIPV6::ORIGDSTADDR => {
                self.recv_origdstaddr_v6.store(v, Ordering::Relaxed);
                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    /// 处理 SOL_IPV6 级别的 getsockopt。
    pub(super) fn get_ipv6_option(
        &self,
        opt: PIPV6,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        if value.len() < core::mem::size_of::<i32>() {
            return Err(SystemError::EINVAL);
        }

        let v = match opt {
            PIPV6::RECVTCLASS => {
                if self.recv_tclass.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            PIPV6::RECVERR | PIPV6::RECVERR_RFC4884 => {
                if self.recv_err_v6.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            PIPV6::ORIGDSTADDR => {
                if self.recv_origdstaddr_v6.load(Ordering::Relaxed) {
                    1i32
                } else {
                    0i32
                }
            }
            _ => return Err(SystemError::ENOPROTOOPT),
        };

        value[..4].copy_from_slice(&v.to_ne_bytes());
        Ok(core::mem::size_of::<i32>())
    }
}

bitflags! {
    pub struct UdpSocketOptions: u32 {
        const ZERO = 0;        /* No UDP options */
        const UDP_CORK = 1;         /* Never send partially complete segments */
        const UDP_ENCAP = 100;      /* Set the socket to accept encapsulated packets */
        const UDP_NO_CHECK6_TX = 101; /* Disable sending checksum for UDP6X */
        const UDP_NO_CHECK6_RX = 102; /* Disable accepting checksum for UDP6 */
        const UDP_SEGMENT = 103;    /* Set GSO segmentation size */
        const UDP_GRO = 104;        /* This socket can receive UDP GRO packets */

        const UDPLITE_SEND_CSCOV = 10; /* sender partial coverage (as sent)      */
        const UDPLITE_RECV_CSCOV = 11; /* receiver partial coverage (threshold ) */
    }
}

bitflags! {
    pub struct UdpEncapTypes: u8 {
        const ZERO = 0;
        const ESPINUDP_NON_IKE = 1;     // draft-ietf-ipsec-nat-t-ike-00/01
        const ESPINUDP = 2;             // draft-ietf-ipsec-udp-encaps-06
        const L2TPINUDP = 3;            // rfc2661
        const GTP0 = 4;                 // GSM TS 09.60
        const GTP1U = 5;                // 3GPP TS 29.060
        const RXRPC = 6;
        const ESPINTCP = 7;             // Yikes, this is really xfrm encap types.
    }
}
