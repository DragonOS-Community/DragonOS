/// Linux UAPI: ARP硬件类型 (include/uapi/linux/if_arp.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive)]
#[repr(u16)]
pub enum ArpHrd {
    /// NET/ROM pseudo
    Netrom = 0,
    /// Ethernet 10Mbps
    Ethernet = 1,
    /// Experimental Ethernet
    Eether = 2,
    /// AX.25 Level 2
    Ax25 = 3,
    /// PROnet token ring
    Pronet = 4,
    /// Chaosnet
    Chaos = 5,
    /// IEEE 802.2 Ethernet/TR/TB
    Ieee802 = 6,
    /// ARCnet
    Arcnet = 7,
    /// APPLEtalk
    Appletlk = 8,
    /// Frame Relay DLCI
    Dlci = 15,
    /// ATM
    Atm = 19,
    /// Metricom STRIP
    Metricom = 23,
    /// IEEE 1394 IPv4 - RFC 2734
    Ieee1394 = 24,
    /// EUI-64
    Eui64 = 27,
    /// InfiniBand
    Infiniband = 32,
    /// SLIP
    Slip = 256,
    /// CSLIP
    Cslip = 257,
    /// SLIP6
    Slip6 = 258,
    /// CSLIP6
    Cslip6 = 259,
    /// Notional KISS type
    Rsrvd = 260,
    /// Adaptive
    Adapt = 264,
    /// ROSE
    Rose = 270,
    /// CCITT X.25
    X25 = 271,
    /// Boards with X.25 in firmware
    Hwx25 = 272,
    /// Controller Area Network
    Can = 280,
    /// MCTP
    Mctp = 290,
    /// PPP
    Ppp = 512,
    /// Cisco HDLC
    Cisco = 513,
    /// LAPB
    Lapb = 516,
    /// Digital's DDCMP protocol
    Ddcmp = 517,
    /// Raw HDLC
    Rawhdlc = 518,
    /// Raw IP
    Rawip = 519,
    /// IPIP tunnel
    Tunnel = 768,
    /// IP6IP6 tunnel
    Tunnel6 = 769,
    /// Frame Relay Access Device
    Frad = 770,
    /// SKIP vif
    Skip = 771,
    /// Loopback device
    Loopback = 772,
    /// Localtalk device
    Localtlk = 773,
    /// Fiber Distributed Data Interface
    Fddi = 774,
    /// AP1000 BIF
    Bif = 775,
    /// sit0 device - IPv6-in-IPv4
    Sit = 776,
    /// IP over DDP tunneller
    Ipddp = 777,
    /// GRE over IP
    Ipgre = 778,
    /// PIMSM register interface
    Pimreg = 779,
    /// High Performance Parallel Interface
    Hippippi = 780,
    /// Nexus 64Mbps Ash
    Ash = 781,
    /// Acorn Econet
    Econet = 782,
    /// Linux-IrDA
    Irda = 783,
    /// Point to point fibrechannel
    Fcpp = 784,
    /// Fibrechannel arbitrated loop
    Fcal = 785,
    /// Fibrechannel public loop
    Fcpl = 786,
    /// Fibrechannel fabric
    Fcfabric = 787,
    /// Magic type ident for TR
    Ieee802Tr = 800,
    /// IEEE 802.11
    Ieee80211 = 801,
    /// IEEE 802.11 + Prism2 header
    Ieee80211Prism = 802,
    /// IEEE 802.11 + radiotap header
    Ieee80211Radiotap = 803,
    /// IEEE 802.15.4
    Ieee802154 = 804,
    /// IEEE 802.15.4 network monitor
    Ieee802154Monitor = 805,
    /// PhoNet media type
    Phonet = 820,
    /// PhoNet pipe header
    PhonetPipe = 821,
    /// CAIF media type
    Caif = 822,
    /// GRE over IPv6
    Ip6gre = 823,
    /// Netlink header
    Netlink = 824,
    /// IPv6 over LoWPAN
    Lowpan = 825,
    /// Vsock monitor header
    Vsockmon = 826,
    /// Void type, nothing is known
    Void = 0xFFFF,
    /// zero header length
    None = 0xFFFE,
}

impl ArpHrd {
    /// 转换为u16
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// 从u16创建ArpHrd
    #[allow(dead_code)]
    pub fn from_u16(value: u16) -> Option<Self> {
        <Self as num_traits::FromPrimitive>::from_u16(value)
    }
}

bitflags::bitflags! {
    /// Linux UAPI: ARP标志位 (include/uapi/linux/if_arp.h)
    /// ARP条目标志位
    pub struct ArpFlags: u16 {
        /// 完成的条目 (硬件地址有效)
        const COM = 0x02;
        /// 永久条目
        const PERM = 0x04;
        /// 发布条目 (用于代理ARP)
        const PUBL = 0x08;
        /// 请求了trailers
        const USETRAILERS = 0x10;
        /// 使用netmask (仅用于代理条目)
        const NETMASK = 0x20;
        /// 不响应此地址
        const DONTPUB = 0x40;
    }
}
