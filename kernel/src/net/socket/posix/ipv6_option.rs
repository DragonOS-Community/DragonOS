/// SOL_IPV6 层选项 (include/uapi/linux/in6.h)
///
/// 参考 Linux 6.6.21 `include/uapi/linux/in6.h`。
/// 这里集中定义 IPv6 socket option 的 optname 值，供 inet/raw 等实现复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum Ipv6Option {
    ADDRFORM = 1,

    // RFC2292 (legacy)
    RFC2292_PKTINFO = 2,
    RFC2292_HOPOPTS = 3,
    RFC2292_DSTOPTS = 4,
    RFC2292_RTHDR = 5,
    RFC2292_PKTOPTIONS = 6,
    CHECKSUM = 7,
    RFC2292_HOPLIMIT = 8,

    NEXTHOP = 9,
    AUTHHDR = 10,
    FLOWINFO = 11,

    UNICAST_HOPS = 16,
    MULTICAST_IF = 17,
    MULTICAST_HOPS = 18,
    MULTICAST_LOOP = 19,
    ADD_MEMBERSHIP = 20,
    DROP_MEMBERSHIP = 21,
    ROUTER_ALERT = 22,
    MTU_DISCOVER = 23,
    MTU = 24,
    RECVERR = 25,
    V6ONLY = 26,
    JOIN_ANYCAST = 27,
    LEAVE_ANYCAST = 28,
    MULTICAST_ALL = 29,
    ROUTER_ALERT_ISOLATE = 30,
    RECVERR_RFC4884 = 31,

    FLOWLABEL_MGR = 32,
    FLOWINFO_SEND = 33,
    IPSEC_POLICY = 34,
    XFRM_POLICY = 35,
    HDRINCL = 36,

    // RFC3542 (advanced API)
    RECVPKTINFO = 49,
    PKTINFO = 50,
    RECVHOPLIMIT = 51,
    HOPLIMIT = 52,
    RECVHOPOPTS = 53,
    HOPOPTS = 54,
    RTHDRDSTOPTS = 55,
    RECVRTHDR = 56,
    RTHDR = 57,
    RECVDSTOPTS = 58,
    DSTOPTS = 59,
    RECVPATHMTU = 60,
    PATHMTU = 61,
    DONTFRAG = 62,

    RECVTCLASS = 66,
    TCLASS = 67,

    AUTOFLOWLABEL = 70,
    ADDR_PREFERENCES = 72,
    MINHOPCOUNT = 73,
    ORIGDSTADDR = 74,
    TRANSPARENT = 75,
    UNICAST_IF = 76,
    RECVFRAGSIZE = 77,
    FREEBIND = 78,
}

/// Linux: `#define IPV6_RECVORIGDSTADDR IPV6_ORIGDSTADDR`
#[allow(dead_code)]
pub const IPV6_RECVORIGDSTADDR: u32 = Ipv6Option::ORIGDSTADDR as u32;

impl TryFrom<u32> for Ipv6Option {
    type Error = system_error::SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        <Self as FromPrimitive>::from_u32(value).ok_or(system_error::SystemError::EINVAL)
    }
}

/// `IPV6_MTU_DISCOVER` 的取值 (include/uapi/linux/in6.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum Ipv6PmtuDiscover {
    DONT = 0,
    WANT = 1,
    DO = 2,
    PROBE = 3,
    INTERFACE = 4,
    OMIT = 5,
}

#[allow(dead_code)]
impl Ipv6PmtuDiscover {
    /// 返回 Linux UAPI 中的原始数值。
    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

impl TryFrom<u32> for Ipv6PmtuDiscover {
    type Error = system_error::SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        <Self as FromPrimitive>::from_u32(value).ok_or(system_error::SystemError::EINVAL)
    }
}

bitflags! {
    /// `IPV6_ADDR_PREFERENCES` 的 bitmask (include/uapi/linux/in6.h)
    pub struct Ipv6AddrPreferences: u32 {
        const PREFER_SRC_TMP = 0x0001;
        const PREFER_SRC_PUBLIC = 0x0002;
        const PREFER_SRC_PUBTMP_DEFAULT = 0x0100;
        const PREFER_SRC_COA = 0x0004;
        const PREFER_SRC_HOME = 0x0400;
        const PREFER_SRC_CGA = 0x0008;
        const PREFER_SRC_NONCGA = 0x0800;
    }
}
