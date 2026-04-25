use alloc::string::String;
use core::fmt::Debug;

use super::constants::{SYSCTL_RMEM_MAX, SYSCTL_WMEM_MAX};

/// 默认 IP TTL/Hop Limit (RFC 1340)
pub(super) const DEFAULT_IP_TTL: u8 = 64;

/// ICMP 过滤器 (32位位掩码)
///
/// 用于过滤特定 ICMP 类型的数据包
#[derive(Debug, Clone, Default)]
pub struct IcmpFilter {
    /// 位掩码：第 N 位为 1 表示过滤掉 ICMP type N (0-31)
    mask: u32,
}

/// ICMPv6 过滤器（对应 Linux UAPI 的 `struct icmp6_filter`）
///
/// `icmp6_filt[i]` 的 bit 为 1 表示阻止对应 type，0 表示允许。
#[derive(Clone, Copy, Default)]
pub struct Icmp6Filter {
    pub icmp6_filt: [u32; 8],
}

impl Icmp6Filter {
    #[inline]
    pub fn should_filter(&self, icmp_type: u8) -> bool {
        let idx = (icmp_type as usize) / 32;
        let bit = (icmp_type as usize) % 32;
        if idx >= self.icmp6_filt.len() {
            return false;
        }
        (self.icmp6_filt[idx] & (1u32 << bit)) != 0
    }
}

impl IcmpFilter {
    #[allow(dead_code)]
    pub fn new(mask: u32) -> Self {
        Self { mask }
    }

    /// 检查是否应该过滤该 ICMP 类型
    ///
    /// # 参数
    /// - `icmp_type`: ICMP 消息类型 (0-255)
    ///
    /// # 返回
    /// - `true`: 应该过滤掉（丢弃）
    /// - `false`: 应该接收
    pub fn should_filter(&self, icmp_type: u8) -> bool {
        if icmp_type >= 32 {
            return false; // 超出范围的类型不过滤
        }
        (self.mask & (1 << icmp_type)) != 0
    }

    pub fn set_mask(&mut self, mask: u32) {
        self.mask = mask;
    }

    pub fn get_mask(&self) -> u32 {
        self.mask
    }
}

/// Raw socket 选项
#[derive(Clone)]
pub struct RawSocketOptions {
    /// IP_HDRINCL: 用户是否提供完整 IP 头
    pub ip_hdrincl: bool,
    /// IP_TOS: Type of Service
    pub ip_tos: u8,
    /// IP_TTL: Time to Live
    pub ip_ttl: u8,
    /// IP_PKTINFO: 接收时是否返回 in_pktinfo 控制消息
    pub recv_pktinfo_v4: bool,
    /// IP_RECVTOS: 接收时是否返回 IP_TOS 控制消息
    pub recv_tos: bool,
    /// IP_RECVTTL: 接收时是否返回 IP_TTL 控制消息
    pub recv_ttl: bool,
    /// IPV6_RECVPKTINFO: 接收时是否返回 in6_pktinfo 控制消息
    pub recv_pktinfo_v6: bool,
    /// IPV6_RECVTCLASS: 接收时是否返回 IPV6_TCLASS 控制消息
    pub recv_tclass: bool,
    /// IPV6_RECVHOPLIMIT: 接收时是否返回 IPV6_HOPLIMIT 控制消息
    pub recv_hoplimit: bool,
    /// ICMP_FILTER: ICMP 类型过滤位掩码 (仅 IPPROTO_ICMP)
    pub icmp_filter: IcmpFilter,

    /// ICMP6_FILTER: ICMPv6 类型过滤器 (仅 IPPROTO_ICMPV6)
    pub icmp6_filter: Icmp6Filter,

    /// IPV6_CHECKSUM: -1 表示不校验/不计算；否则为校验和字段在 payload 内的偏移(单位：字节)
    pub ipv6_checksum: i32,

    /// SO_SNDBUF: 返回给 getsockopt 的 sk_sndbuf（Linux 会将 setsockopt 的值 *2 后存储）
    pub sock_sndbuf: u32,
    /// SO_RCVBUF: 返回给 getsockopt 的 sk_rcvbuf（Linux 会将 setsockopt 的值 *2 后存储）
    pub sock_rcvbuf: u32,

    /// SO_BINDTODEVICE: 绑定的设备名（不含 '\0'）
    pub bind_to_device: Option<String>,

    /// SO_LINGER
    pub linger_onoff: i32,
    pub linger_linger: i32,

    /// SO_ATTACH_FILTER/ SO_DETACH_FILTER
    pub filter_attached: bool,
}

impl Debug for RawSocketOptions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawSocketOptions").finish()
    }
}

impl Default for RawSocketOptions {
    fn default() -> Self {
        Self {
            ip_hdrincl: false,
            ip_tos: 0,
            ip_ttl: DEFAULT_IP_TTL,
            recv_pktinfo_v4: false,
            recv_tos: false,
            recv_ttl: false,
            recv_pktinfo_v6: false,
            recv_tclass: false,
            recv_hoplimit: false,
            icmp_filter: IcmpFilter::default(),
            icmp6_filter: Icmp6Filter::default(),
            ipv6_checksum: -1,

            // Linux 语义：内核存储的 sk_sndbuf/sk_rcvbuf 是用户设置值的 2 倍。
            // 初始值设为 sysctl_*mem_max * 2，与 Linux 默认行为一致。
            sock_sndbuf: SYSCTL_WMEM_MAX.saturating_mul(2),
            sock_rcvbuf: SYSCTL_RMEM_MAX.saturating_mul(2),
            bind_to_device: None,
            linger_onoff: 0,
            linger_linger: 0,
            filter_attached: false,
        }
    }
}
