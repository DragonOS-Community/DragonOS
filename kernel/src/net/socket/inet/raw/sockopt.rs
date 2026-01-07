use alloc::string::String;

use smoltcp::wire::IpProtocol;
use system_error::SystemError;

use super::constants::{
    ICMPV6_CHECKSUM_OFFSET, SOCK_MIN_RCVBUF, SOCK_MIN_SNDBUF, SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX,
};
use super::options::DEFAULT_IP_TTL;
use super::{Icmp6Filter, RawSocket};
use crate::net::socket::{IpOption, IFNAMSIZ, PIPV6, PRAW, PSO};

fn sock_buf_u32_from_opt(val: &[u8]) -> Result<u32, SystemError> {
    if val.len() < 4 {
        return Err(SystemError::EINVAL);
    }
    Ok(u32::from_ne_bytes([val[0], val[1], val[2], val[3]]))
}

fn clamp_sock_buf(val_u32: u32, sysctl_max: u32, sock_min: u32) -> u32 {
    // Linux: val = min_t(u32, val, sysctl_*mem_max)
    let mut val = core::cmp::min(val_u32, sysctl_max);
    // Ensure val*2 won't overflow signed int logic.
    val = core::cmp::min(val, (i32::MAX as u32) / 2);
    let doubled = val.saturating_mul(2);
    core::cmp::max(doubled, sock_min)
}

fn read_i32_opt(val: &[u8]) -> Option<i32> {
    if val.len() >= 4 {
        Some(i32::from_ne_bytes([val[0], val[1], val[2], val[3]]))
    } else {
        None
    }
}

impl RawSocket {
    // ========================================================================
    // getsockopt 辅助方法 - 按 level 分组
    // ========================================================================

    pub(super) fn option_socket_level(
        &self,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match PSO::try_from(name as u32) {
            Ok(PSO::SNDBUF) => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.options.read().sock_sndbuf;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(PSO::RCVBUF) => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.options.read().sock_rcvbuf;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(PSO::BINDTODEVICE) => {
                let name = self
                    .options
                    .read()
                    .bind_to_device
                    .clone()
                    .unwrap_or_default();
                let need = core::cmp::min(name.len() + 1, IFNAMSIZ);
                if value.len() < need {
                    return Err(SystemError::EINVAL);
                }
                if need == 0 {
                    return Ok(0);
                }
                let bytes = name.as_bytes();
                let copy_len = core::cmp::min(bytes.len(), need.saturating_sub(1));
                value[..copy_len].copy_from_slice(&bytes[..copy_len]);
                value[copy_len] = 0;
                Ok(need)
            }
            Ok(PSO::LINGER) => {
                if value.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                let opts = self.options.read();
                value[..4].copy_from_slice(&opts.linger_onoff.to_ne_bytes());
                value[4..8].copy_from_slice(&opts.linger_linger.to_ne_bytes());
                Ok(8)
            }
            Ok(PSO::ACCEPTCONN) => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = 0i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(PSO::DETACH_FILTER) => Err(SystemError::ENOPROTOOPT),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    pub(super) fn option_raw_level(
        &self,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match PRAW::try_from(name as u32) {
            Ok(PRAW::ICMP_FILTER) => {
                if self.protocol != IpProtocol::Icmp {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let mask = self.options.read().icmp_filter.get_mask();
                value[..4].copy_from_slice(&mask.to_ne_bytes());
                Ok(4)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    pub(super) fn option_ip_level(
        &self,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match IpOption::try_from(name as u32) {
            Ok(IpOption::HDRINCL) => {
                let v = if self.options.read().ip_hdrincl {
                    1i32
                } else {
                    0i32
                };
                let len = core::cmp::min(value.len(), 4);
                value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                Ok(len)
            }
            Ok(IpOption::TOS) => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.options.read().ip_tos as i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(IpOption::TTL) => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.options.read().ip_ttl as i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(IpOption::PKTINFO) => {
                let v = if self.options.read().recv_pktinfo_v4 {
                    1i32
                } else {
                    0i32
                };
                let len = core::cmp::min(value.len(), 4);
                value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                Ok(len)
            }
            Ok(IpOption::RECVTTL) => {
                let v = if self.options.read().recv_ttl {
                    1i32
                } else {
                    0i32
                };
                let len = core::cmp::min(value.len(), 4);
                value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                Ok(len)
            }
            Ok(IpOption::RECVTOS) => {
                let v = if self.options.read().recv_tos {
                    1i32
                } else {
                    0i32
                };
                let len = core::cmp::min(value.len(), 4);
                value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                Ok(len)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    pub(super) fn option_ipv6_level(
        &self,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        if self.ip_version != smoltcp::wire::IpVersion::Ipv6 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        match PIPV6::try_from(name as u32) {
            Ok(PIPV6::CHECKSUM) => {
                let v = if self.protocol == IpProtocol::Icmpv6 {
                    ICMPV6_CHECKSUM_OFFSET
                } else {
                    self.options.read().ipv6_checksum
                };
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(PIPV6::UNICAST_HOPS) => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.options.read().ip_ttl as i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(PIPV6::TCLASS) => {
                if value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let v = self.options.read().ip_tos as i32;
                value[..4].copy_from_slice(&v.to_ne_bytes());
                Ok(4)
            }
            Ok(PIPV6::RECVPKTINFO) => {
                let v = if self.options.read().recv_pktinfo_v6 {
                    1i32
                } else {
                    0i32
                };
                let len = core::cmp::min(value.len(), 4);
                value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                Ok(len)
            }
            Ok(PIPV6::RECVHOPLIMIT) => {
                let v = if self.options.read().recv_hoplimit {
                    1i32
                } else {
                    0i32
                };
                let len = core::cmp::min(value.len(), 4);
                value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                Ok(len)
            }
            Ok(PIPV6::RECVTCLASS) => {
                let v = if self.options.read().recv_tclass {
                    1i32
                } else {
                    0i32
                };
                let len = core::cmp::min(value.len(), 4);
                value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
                Ok(len)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    pub(super) fn option_icmpv6_level(
        &self,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        if self.ip_version != smoltcp::wire::IpVersion::Ipv6 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if self.protocol != IpProtocol::Icmpv6 {
            return Err(SystemError::ENOPROTOOPT);
        }
        // ICMP6_FILTER = 1
        if name != 1 {
            return Err(SystemError::ENOPROTOOPT);
        }
        let filt = self.options.read().icmp6_filter;
        let bytes = unsafe {
            core::slice::from_raw_parts(
                filt.icmp6_filt.as_ptr() as *const u8,
                core::mem::size_of_val(&filt.icmp6_filt),
            )
        };
        value[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    // ========================================================================
    // setsockopt 辅助方法 - 按 level 分组
    // ========================================================================

    pub(super) fn set_option_socket_level(
        &self,
        name: usize,
        val: &[u8],
    ) -> Result<(), SystemError> {
        match PSO::try_from(name as u32) {
            Ok(PSO::SNDBUF) => {
                let v = sock_buf_u32_from_opt(val)?;
                let newv = clamp_sock_buf(v, SYSCTL_WMEM_MAX, SOCK_MIN_SNDBUF);
                self.options.write().sock_sndbuf = newv;
                Ok(())
            }
            Ok(PSO::RCVBUF) => {
                let v = sock_buf_u32_from_opt(val)?;
                let newv = clamp_sock_buf(v, SYSCTL_RMEM_MAX, SOCK_MIN_RCVBUF);
                self.options.write().sock_rcvbuf = newv;
                Ok(())
            }
            Ok(PSO::BINDTODEVICE) => {
                let end = val.iter().position(|&b| b == 0).unwrap_or(val.len());
                let name_bytes = &val[..end];
                if name_bytes.is_empty() {
                    self.options.write().bind_to_device = None;
                    return Ok(());
                }
                let name = core::str::from_utf8(name_bytes).map_err(|_| SystemError::EINVAL)?;
                let found = self
                    .netns
                    .device_list()
                    .values()
                    .any(|iface| iface.iface_name() == name);
                if !found {
                    return Err(SystemError::ENODEV);
                }
                self.options.write().bind_to_device = Some(String::from(name));
                Ok(())
            }
            Ok(PSO::DETACH_FILTER) => {
                let mut opts = self.options.write();
                if !opts.filter_attached {
                    return Err(SystemError::ENOENT);
                }
                opts.filter_attached = false;
                Ok(())
            }
            Ok(PSO::LINGER) => {
                if val.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                let l_onoff = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                let l_linger = i32::from_ne_bytes([val[4], val[5], val[6], val[7]]);
                if l_linger < 0 {
                    return Err(SystemError::EINVAL);
                }
                let mut opts = self.options.write();
                opts.linger_onoff = if l_onoff != 0 { 1 } else { 0 };
                opts.linger_linger = l_linger;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(super) fn set_option_raw_level(&self, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match PRAW::try_from(name as u32) {
            Ok(PRAW::ICMP_FILTER) => {
                if self.protocol != IpProtocol::Icmp {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if val.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                let mask = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                self.options.write().icmp_filter.set_mask(mask);
                Ok(())
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    pub(super) fn set_option_ip_level(&self, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match IpOption::try_from(name as u32) {
            Ok(IpOption::HDRINCL) => {
                let enable = val.first().copied().unwrap_or(0) != 0;
                self.options.write().ip_hdrincl = enable;
                Ok(())
            }
            Ok(IpOption::TOS) => {
                let v = read_i32_opt(val).unwrap_or(val.first().copied().unwrap_or(0) as i32);
                if !(0..=255).contains(&v) {
                    return Err(SystemError::EINVAL);
                }
                self.options.write().ip_tos = v as u8;
                Ok(())
            }
            Ok(IpOption::TTL) => {
                let v = read_i32_opt(val)
                    .unwrap_or(val.first().copied().unwrap_or(DEFAULT_IP_TTL) as i32);
                if !(0..=255).contains(&v) {
                    return Err(SystemError::EINVAL);
                }
                self.options.write().ip_ttl = v as u8;
                Ok(())
            }
            Ok(IpOption::PKTINFO) => {
                let enable = val.first().copied().unwrap_or(0) != 0;
                self.options.write().recv_pktinfo_v4 = enable;
                Ok(())
            }
            Ok(IpOption::RECVTTL) => {
                let enable = val.first().copied().unwrap_or(0) != 0;
                self.options.write().recv_ttl = enable;
                Ok(())
            }
            Ok(IpOption::RECVTOS) => {
                let enable = val.first().copied().unwrap_or(0) != 0;
                self.options.write().recv_tos = enable;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(super) fn set_option_ipv6_level(&self, name: usize, val: &[u8]) -> Result<(), SystemError> {
        if self.ip_version != smoltcp::wire::IpVersion::Ipv6 {
            return Err(SystemError::ENOPROTOOPT);
        }
        match PIPV6::try_from(name as u32) {
            Ok(PIPV6::CHECKSUM) => {
                if self.protocol == IpProtocol::Icmpv6 {
                    return Err(SystemError::EINVAL);
                }
                let v = read_i32_opt(val).ok_or(SystemError::EINVAL)?;
                if v != -1 {
                    if v < 0 {
                        return Err(SystemError::EINVAL);
                    }
                    if (v & 1) != 0 {
                        return Err(SystemError::EINVAL);
                    }
                }
                self.options.write().ipv6_checksum = v;
                Ok(())
            }
            Ok(PIPV6::UNICAST_HOPS) => {
                let v = read_i32_opt(val).ok_or(SystemError::EINVAL)?;
                if v == -1 {
                    return Ok(());
                }
                if !(0..=255).contains(&v) {
                    return Err(SystemError::EINVAL);
                }
                self.options.write().ip_ttl = v as u8;
                Ok(())
            }
            Ok(PIPV6::TCLASS) => {
                let v = read_i32_opt(val).ok_or(SystemError::EINVAL)?;
                if !(0..=255).contains(&v) {
                    return Err(SystemError::EINVAL);
                }
                self.options.write().ip_tos = v as u8;
                Ok(())
            }
            Ok(PIPV6::RECVPKTINFO) => {
                let enable = val.first().copied().unwrap_or(0) != 0;
                self.options.write().recv_pktinfo_v6 = enable;
                Ok(())
            }
            Ok(PIPV6::RECVHOPLIMIT) => {
                let enable = val.first().copied().unwrap_or(0) != 0;
                self.options.write().recv_hoplimit = enable;
                Ok(())
            }
            Ok(PIPV6::RECVTCLASS) => {
                let enable = val.first().copied().unwrap_or(0) != 0;
                self.options.write().recv_tclass = enable;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(super) fn set_option_icmpv6_level(
        &self,
        name: usize,
        val: &[u8],
    ) -> Result<(), SystemError> {
        if self.ip_version != smoltcp::wire::IpVersion::Ipv6 {
            return Err(SystemError::ENOPROTOOPT);
        }
        if self.protocol != IpProtocol::Icmpv6 {
            return Err(SystemError::ENOPROTOOPT);
        }
        // ICMP6_FILTER = 1
        if name != 1 {
            return Err(SystemError::ENOPROTOOPT);
        }
        let need = core::mem::size_of::<[u32; 8]>();
        if val.len() < need {
            return Err(SystemError::EINVAL);
        }
        let mut filt = [0u32; 8];
        for (i, filt_elem) in filt.iter_mut().enumerate() {
            let off = i * 4;
            *filt_elem = u32::from_ne_bytes([val[off], val[off + 1], val[off + 2], val[off + 3]]);
        }
        self.options.write().icmp6_filter = Icmp6Filter { icmp6_filt: filt };
        Ok(())
    }
}
