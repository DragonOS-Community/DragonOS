// pub const SOL_SOCKET: u8 = 1,
// bitflags::bitflags! {
//     pub struct OptionsLevel: u32 {
//         const IP = 0,
//         // const SOL_ICMP = 1, // No-no-no! Due to Linux :-) we cannot
//         const SOCKET = 1,
//         const TCP = 6,
//         const UDP = 17,
//         const IPV6 = 41,
//         const ICMPV6 = 58,
//         const SCTP = 132,
//         const UDPLITE = 136, // UDP-Lite (RFC 3828)
//         const RAW = 255,
//         const IPX = 256,
//         const AX25 = 257,
//         const ATALK = 258,
//         const NETROM = 259,
//         const ROSE = 260,
//         const DECNET = 261,
//         const X25 = 262,
//         const PACKET = 263,
//         const ATM = 264, // ATM layer (cell level)
//         const AAL = 265, // ATM Adaption Layer (packet level)
//         const IRDA = 266,
//         const NETBEUI = 267,
//         const LLC = 268,
//         const DCCP = 269,
//         const NETLINK = 270,
//         const TIPC = 271,
//         const RXRPC = 272,
//         const PPPOL2TP = 273,
//         const BLUETOOTH = 274,
//         const PNPIPE = 275,
//         const RDS = 276,
//         const IUCV = 277,
//         const CAIF = 278,
//         const ALG = 279,
//         const NFC = 280,
//         const KCM = 281,
//         const TLS = 282,
//         const XDP = 283,
//         const MPTCP = 284,
//         const MCTP = 285,
//         const SMC = 286,
//         const VSOCK = 287,
//     }
// }

/// # SOL (Socket Option Level)
/// Setsockoptions(2) level. Thanks to BSD these must match IPPROTO_xxx
/// ## Reference
/// - [Setsockoptions(2) level](https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/socket.h#345)
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum OptionsLevel {
    IP = 0,
    SOCKET = 1,
    // ICMP = 1, No-no-no! Due to Linux :-) we cannot
    TCP = 6,
    UDP = 17,
    IPV6 = 41,
    ICMPV6 = 58,
    SCTP = 132,
    UDPLITE = 136, // UDP-Lite (RFC 3828)
    RAW = 255,
    IPX = 256,
    AX25 = 257,
    ATALK = 258,
    NETROM = 259,
    ROSE = 260,
    DECNET = 261,
    X25 = 262,
    PACKET = 263,
    ATM = 264, // ATM layer (cell level)
    AAL = 265, // ATM Adaption Layer (packet level)
    IRDA = 266,
    NETBEUI = 267,
    LLC = 268,
    DCCP = 269,
    NETLINK = 270,
    TIPC = 271,
    RXRPC = 272,
    PPPOL2TP = 273,
    BLUETOOTH = 274,
    PNPIPE = 275,
    RDS = 276,
    IUCV = 277,
    CAIF = 278,
    ALG = 279,
    NFC = 280,
    KCM = 281,
    TLS = 282,
    XDP = 283,
    MPTCP = 284,
    MCTP = 285,
    SMC = 286,
    VSOCK = 287,
}

impl TryFrom<u32> for OptionsLevel {
    type Error = system_error::SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match <Self as num_traits::FromPrimitive>::from_u32(value) {
            Some(p) => Ok(p),
            None => Err(system_error::SystemError::EPROTONOSUPPORT),
        }
    }
}

impl From<OptionsLevel> for u32 {
    fn from(value: OptionsLevel) -> Self {
        <OptionsLevel as num_traits::ToPrimitive>::to_u32(&value).unwrap()
    }
}
