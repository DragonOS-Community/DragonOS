// bitflags! {
//     // #[derive(PartialEq, Eq, Debug, Clone, Copy)]
//     pub struct Options: u32 {
//         const DEBUG = 1;
//         const REUSEADDR = 2;
//         const TYPE = 3;
//         const ERROR = 4;
//         const DONTROUTE = 5;
//         const BROADCAST = 6;
//         const SNDBUF = 7;
//         const RCVBUF = 8;
//         const SNDBUFFORCE = 32;
//         const RCVBUFFORCE = 33;
//         const KEEPALIVE = 9;
//         const OOBINLINE = 10;
//         const NO_CHECK = 11;
//         const PRIORITY = 12;
//         const LINGER = 13;
//         const BSDCOMPAT = 14;
//         const REUSEPORT = 15;
//         const PASSCRED = 16;
//         const PEERCRED = 17;
//         const RCVLOWAT = 18;
//         const SNDLOWAT = 19;
//         const RCVTIMEO_OLD = 20;
//         const SNDTIMEO_OLD = 21;
//
//         const SECURITY_AUTHENTICATION = 22;
//         const SECURITY_ENCRYPTION_TRANSPORT = 23;
//         const SECURITY_ENCRYPTION_NETWORK = 24;
//
//         const BINDTODEVICE = 25;
//
//         /// 与GET_FILTER相同
//         const ATTACH_FILTER = 26;
//         const DETACH_FILTER = 27;
//
//         const PEERNAME = 28;
//
//         const ACCEPTCONN = 30;
//
//         const PEERSEC = 31;
//         const PASSSEC = 34;
//
//         const MARK = 36;
//
//         const PROTOCOL = 38;
//         const DOMAIN = 39;
//
//         const RXQ_OVFL = 40;
//
//         /// 与SCM_WIFI_STATUS相同
//         const WIFI_STATUS = 41;
//         const PEEK_OFF = 42;
//
//         /* Instruct lower device to use last 4-bytes of skb data as FCS */
//         const NOFCS = 43;
//
//         const LOCK_FILTER = 44;
//         const SELECT_ERR_QUEUE = 45;
//         const BUSY_POLL = 46;
//         const MAX_PACING_RATE = 47;
//         const BPF_EXTENSIONS = 48;
//         const INCOMING_CPU = 49;
//         const ATTACH_BPF = 50;
//         // DETACH_BPF = DETACH_FILTER;
//         const ATTACH_REUSEPORT_CBPF = 51;
//         const ATTACH_REUSEPORT_EBPF = 52;
//
//         const CNX_ADVICE = 53;
//         const SCM_TIMESTAMPING_OPT_STATS = 54;
//         const MEMINFO = 55;
//         const INCOMING_NAPI_ID = 56;
//         const COOKIE = 57;
//         const SCM_TIMESTAMPING_PKTINFO = 58;
//         const PEERGROUPS = 59;
//         const ZEROCOPY = 60;
//         /// 与SCM_TXTIME相同
//         const TXTIME = 61;
//
//         const BINDTOIFINDEX = 62;
//
//         const TIMESTAMP_OLD = 29;
//         const TIMESTAMPNS_OLD = 35;
//         const TIMESTAMPING_OLD = 37;
//         const TIMESTAMP_NEW = 63;
//         const TIMESTAMPNS_NEW = 64;
//         const TIMESTAMPING_NEW = 65;
//
//         const RCVTIMEO_NEW = 66;
//         const SNDTIMEO_NEW = 67;
//
//         const DETACH_REUSEPORT_BPF = 68;
//
//         const PREFER_BUSY_POLL = 69;
//         const BUSY_POLL_BUDGET = 70;
//
//         const NETNS_COOKIE = 71;
//         const BUF_LOCK = 72;
//         const RESERVE_MEM = 73;
//         const TXREHASH = 74;
//         const RCVMARK = 75;
//     }
// }

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum Options {
    DEBUG = 1,
    REUSEADDR = 2,
    TYPE = 3,
    ERROR = 4,
    DONTROUTE = 5,
    BROADCAST = 6,
    SNDBUF = 7,
    RCVBUF = 8,
    SNDBUFFORCE = 32,
    RCVBUFFORCE = 33,
    KEEPALIVE = 9,
    OOBINLINE = 10,
    NO_CHECK = 11,
    PRIORITY = 12,
    LINGER = 13,
    BSDCOMPAT = 14,
    REUSEPORT = 15,
    PASSCRED = 16,
    PEERCRED = 17,
    RCVLOWAT = 18,
    SNDLOWAT = 19,
    RCVTIMEO_OLD = 20,
    SNDTIMEO_OLD = 21,
    SECURITY_AUTHENTICATION = 22,
    SECURITY_ENCRYPTION_TRANSPORT = 23,
    SECURITY_ENCRYPTION_NETWORK = 24,
    BINDTODEVICE = 25,
    /// 与GET_FILTER相同
    ATTACH_FILTER = 26,
    DETACH_FILTER = 27,
    PEERNAME = 28,
    ACCEPTCONN = 30,
    PEERSEC = 31,
    PASSSEC = 34,
    MARK = 36,
    PROTOCOL = 38,
    DOMAIN = 39,
    RXQ_OVFL = 40,
    /// 与SCM_WIFI_STATUS相同
    WIFI_STATUS = 41,
    PEEK_OFF = 42,
    /* Instruct lower device to use last 4-bytes of skb data as FCS */
    NOFCS = 43,
    LOCK_FILTER = 44,
    SELECT_ERR_QUEUE = 45,
    BUSY_POLL = 46,
    MAX_PACING_RATE = 47,
    BPF_EXTENSIONS = 48,
    INCOMING_CPU = 49,
    ATTACH_BPF = 50,
    // DETACH_BPF = DETACH_FILTER,
    ATTACH_REUSEPORT_CBPF = 51,
    ATTACH_REUSEPORT_EBPF = 52,
    CNX_ADVICE = 53,
    SCM_TIMESTAMPING_OPT_STATS = 54,
    MEMINFO = 55,
    INCOMING_NAPI_ID = 56,
    COOKIE = 57,
    SCM_TIMESTAMPING_PKTINFO = 58,
    PEERGROUPS = 59,
    ZEROCOPY = 60,
    /// 与SCM_TXTIME相同
    TXTIME = 61,
    BINDTOIFINDEX = 62,
    TIMESTAMP_OLD = 29,
    TIMESTAMPNS_OLD = 35,
    TIMESTAMPING_OLD = 37,
    TIMESTAMP_NEW = 63,
    TIMESTAMPNS_NEW = 64,
    TIMESTAMPING_NEW = 65,
    RCVTIMEO_NEW = 66,
    SNDTIMEO_NEW = 67,
    DETACH_REUSEPORT_BPF = 68,
    PREFER_BUSY_POLL = 69,
    BUSY_POLL_BUDGET = 70,
    NETNS_COOKIE = 71,
    BUF_LOCK = 72,
    RESERVE_MEM = 73,
    TXREHASH = 74,
    RCVMARK = 75,
}

// bitflags::bitflags! {
//     pub struct Level: i32 {
//         const SOL_SOCKET = 1;
//         const IPPROTO_IP = super::ip::Protocol::IP.bits();
//         const IPPROTO_IPV6 = super::ip::Protocol::IPv6.bits();
//         const IPPROTO_TCP = super::ip::Protocol::TCP.bits();
//     }
// }

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum Type {
    Datagram = 1,
    Stream = 2,
    Raw = 3,
    RDM = 4,
    SeqPacket = 5,
    DCCP = 6,
    Packet = 10,
}

use crate::net::syscall_util::SysArgSocketType;
impl TryFrom<SysArgSocketType> for Type {
    type Error = system_error::SystemError;
    fn try_from(x: SysArgSocketType) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u32(x.types().bits()).ok_or(system_error::SystemError::EINVAL);
    }
}

bitflags! {
    pub struct OptionsLevel: u32 {
        const IP = 0;
        // const SOL_ICMP = 1; // No-no-no! Due to Linux :-) we cannot
        const SOCKET = 1;
        const TCP = 6;
        const UDP = 17;
        const IPV6 = 41;
        const ICMPV6 = 58;
        const SCTP = 132;
        const UDPLITE = 136; // UDP-Lite (RFC 3828)
        const RAW = 255;
        const IPX = 256;
        const AX25 = 257;
        const ATALK = 258;
        const NETROM = 259;
        const ROSE = 260;
        const DECNET = 261;
        const X25 = 262;
        const PACKET = 263;
        const ATM = 264; // ATM layer (cell level)
        const AAL = 265; // ATM Adaption Layer (packet level)
        const IRDA = 266;
        const NETBEUI = 267;
        const LLC = 268;
        const DCCP = 269;
        const NETLINK = 270;
        const TIPC = 271;
        const RXRPC = 272;
        const PPPOL2TP = 273;
        const BLUETOOTH = 274;
        const PNPIPE = 275;
        const RDS = 276;
        const IUCV = 277;
        const CAIF = 278;
        const ALG = 279;
        const NFC = 280;
        const KCM = 281;
        const TLS = 282;
        const XDP = 283;
        const MPTCP = 284;
        const MCTP = 285;
        const SMC = 286;
        const VSOCK = 287;
    }
}
