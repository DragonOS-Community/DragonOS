/// # SOL (Socket Option Level)
/// Setsockoptions(2) level. Thanks to BSD these must match IPPROTO_xxx
/// ## Reference
/// - [Setsockoptions(2) level](https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/socket.h#345)
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum OptionLevel {
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

impl TryFrom<u32> for OptionLevel {
    type Error = system_error::SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match <Self as num_traits::FromPrimitive>::from_u32(value) {
            Some(p) => Ok(p),
            None => Err(system_error::SystemError::EPROTONOSUPPORT),
        }
    }
}

impl From<OptionLevel> for u32 {
    fn from(value: OptionLevel) -> Self {
        <OptionLevel as num_traits::ToPrimitive>::to_u32(&value).unwrap()
    }
}
