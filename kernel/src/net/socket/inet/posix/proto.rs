pub const SOL_SOCKET: u16 = 1;

#[derive(Debug, Clone, Copy, FromPrimitive, ToPrimitive, PartialEq, Eq)]
pub enum IPProtocol {
    /// Dummy protocol for TCP.
    IP = 0,
    /// Internet Control Message Protocol.
    ICMP = 1,
    /// Internet Group Management Protocol.
    IGMP = 2,
    /// IPIP tunnels (older KA9Q tunnels use 94).
    IPIP = 4,
    /// Transmission Control Protocol.
    TCP = 6,
    /// Exterior Gateway Protocol.
    EGP = 8,
    /// PUP protocol.
    PUP = 12,
    /// User Datagram Protocol.
    UDP = 17,
    /// XNS IDP protocol.
    IDP = 22,
    /// SO Transport Protocol Class 4.
    TP = 29,
    /// Datagram Congestion Control Protocol.
    DCCP = 33,
    /// IPv6-in-IPv4 tunnelling.
    IPv6 = 41,
    /// RSVP Protocol.
    RSVP = 46,
    /// Generic Routing Encapsulation. (Cisco GRE) (rfc 1701, 1702)
    GRE = 47,
    /// Encapsulation Security Payload protocol
    ESP = 50,
    /// Authentication Header protocol
    AH = 51,
    /// Multicast Transport Protocol.
    MTP = 92,
    /// IP option pseudo header for BEET
    BEETPH = 94,
    /// Encapsulation Header.
    ENCAP = 98,
    /// Protocol Independent Multicast.
    PIM = 103,
    /// Compression Header Protocol.
    COMP = 108,
    /// Stream Control Transport Protocol
    SCTP = 132,
    /// UDP-Lite protocol (RFC 3828)
    UDPLITE = 136,
    /// MPLS in IP (RFC 4023)
    MPLSINIP = 137,
    /// Ethernet-within-IPv6 Encapsulation
    ETHERNET = 143,
    /// Raw IP packets
    RAW = 255,
    /// Multipath TCP connection
    MPTCP = 262,
}

impl TryFrom<u16> for IPProtocol {
    type Error = system_error::SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match <Self as num_traits::FromPrimitive>::from_u16(value) {
            Some(p) => Ok(p),
            None => Err(system_error::SystemError::EPROTONOSUPPORT),
        }
    }
}

impl From<IPProtocol> for u16 {
    fn from(value: IPProtocol) -> Self {
        <IPProtocol as num_traits::ToPrimitive>::to_u16(&value).unwrap()
    }
}
