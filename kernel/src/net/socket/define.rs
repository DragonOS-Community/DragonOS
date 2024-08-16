bitflags! {
    // #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    pub struct Options: u32 {
        const DEBUG = 1;
        const REUSEADDR = 2;
        const TYPE = 3;
        const ERROR = 4;
        const DONTROUTE = 5;
        const BROADCAST = 6;
        const SNDBUF = 7;
        const RCVBUF = 8;
        const SNDBUFFORCE = 32;
        const RCVBUFFORCE = 33;
        const KEEPALIVE = 9;
        const OOBINLINE = 10;
        const NO_CHECK = 11;
        const PRIORITY = 12;
        const LINGER = 13;
        const BSDCOMPAT = 14;
        const REUSEPORT = 15;
        const PASSCRED = 16;
        const PEERCRED = 17;
        const RCVLOWAT = 18;
        const SNDLOWAT = 19;
        const RCVTIMEO_OLD = 20;
        const SNDTIMEO_OLD = 21;

        const SECURITY_AUTHENTICATION = 22;
        const SECURITY_ENCRYPTION_TRANSPORT = 23;
        const SECURITY_ENCRYPTION_NETWORK = 24;

        const BINDTODEVICE = 25;

        /// 与GET_FILTER相同
        const ATTACH_FILTER = 26;
        const DETACH_FILTER = 27;

        const PEERNAME = 28;

        const ACCEPTCONN = 30;

        const PEERSEC = 31;
        const PASSSEC = 34;

        const MARK = 36;

        const PROTOCOL = 38;
        const DOMAIN = 39;

        const RXQ_OVFL = 40;

        /// 与SCM_WIFI_STATUS相同
        const WIFI_STATUS = 41;
        const PEEK_OFF = 42;

        /* Instruct lower device to use last 4-bytes of skb data as FCS */
        const NOFCS = 43;

        const LOCK_FILTER = 44;
        const SELECT_ERR_QUEUE = 45;
        const BUSY_POLL = 46;
        const MAX_PACING_RATE = 47;
        const BPF_EXTENSIONS = 48;
        const INCOMING_CPU = 49;
        const ATTACH_BPF = 50;
        // DETACH_BPF = DETACH_FILTER;
        const ATTACH_REUSEPORT_CBPF = 51;
        const ATTACH_REUSEPORT_EBPF = 52;

        const CNX_ADVICE = 53;
        const SCM_TIMESTAMPING_OPT_STATS = 54;
        const MEMINFO = 55;
        const INCOMING_NAPI_ID = 56;
        const COOKIE = 57;
        const SCM_TIMESTAMPING_PKTINFO = 58;
        const PEERGROUPS = 59;
        const ZEROCOPY = 60;
        /// 与SCM_TXTIME相同
        const TXTIME = 61;

        const BINDTOIFINDEX = 62;

        const TIMESTAMP_OLD = 29;
        const TIMESTAMPNS_OLD = 35;
        const TIMESTAMPING_OLD = 37;
        const TIMESTAMP_NEW = 63;
        const TIMESTAMPNS_NEW = 64;
        const TIMESTAMPING_NEW = 65;

        const RCVTIMEO_NEW = 66;
        const SNDTIMEO_NEW = 67;

        const DETACH_REUSEPORT_BPF = 68;

        const PREFER_BUSY_POLL = 69;
        const BUSY_POLL_BUDGET = 70;

        const NETNS_COOKIE = 71;
        const BUF_LOCK = 72;
        const RESERVE_MEM = 73;
        const TXREHASH = 74;
        const RCVMARK = 75;
    }
}

// bitflags::bitflags! {
//     pub struct Level: i32 {
//         const SOL_SOCKET = 1;
//         const IPPROTO_IP = super::ip::Protocol::IP.bits();
//         const IPPROTO_IPV6 = super::ip::Protocol::IPv6.bits();
//         const IPPROTO_TCP = super::ip::Protocol::TCP.bits();
//     }
// }

bitflags::bitflags! {
    // #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    pub struct Types: u32 {
        const DGRAM     = 1;
        const STREAM    = 2;
        const RAW       = 3;
        const RDM       = 4;
        const SEQPACKET = 5;
        const DCCP      = 6;
        const PACKET    = 10;

        const NONBLOCK  = crate::filesystem::vfs::file::FileMode::O_NONBLOCK.bits();
        const CLOEXEC   = crate::filesystem::vfs::file::FileMode::O_CLOEXEC.bits();
    }
}

impl Types {
    #[inline(always)]
    pub fn types(&self) -> Types {
        Types::from_bits(self.bits() & 0b_1111).unwrap()
    }

    #[inline(always)]
    pub fn is_nonblock(&self) -> bool {
        self.contains(Types::NONBLOCK)
    }

    #[inline(always)]
    pub fn is_cloexec(&self) -> bool {
        self.contains(Types::CLOEXEC)
    }
}

/// @brief 地址族的枚举
///
/// 参考：https://code.dragonos.org.cn/xref/linux-5.19.10/include/linux/socket.h#180
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum AddressFamily {
    /// AF_UNSPEC 表示地址族未指定
    Unspecified = 0,
    /// AF_UNIX 表示Unix域的socket (与AF_LOCAL相同)
    Unix = 1,
    ///  AF_INET 表示IPv4的socket
    INet = 2,
    /// AF_AX25 表示AMPR AX.25的socket
    AX25 = 3,
    /// AF_IPX 表示IPX的socket
    IPX = 4,
    /// AF_APPLETALK 表示Appletalk的socket
    Appletalk = 5,
    /// AF_NETROM 表示AMPR NET/ROM的socket
    Netrom = 6,
    /// AF_BRIDGE 表示多协议桥接的socket
    Bridge = 7,
    /// AF_ATMPVC 表示ATM PVCs的socket
    Atmpvc = 8,
    /// AF_X25 表示X.25的socket
    X25 = 9,
    /// AF_INET6 表示IPv6的socket
    INet6 = 10,
    /// AF_ROSE 表示AMPR ROSE的socket
    Rose = 11,
    /// AF_DECnet Reserved for DECnet project
    Decnet = 12,
    /// AF_NETBEUI Reserved for 802.2LLC project
    Netbeui = 13,
    /// AF_SECURITY 表示Security callback的伪AF
    Security = 14,
    /// AF_KEY 表示Key management API
    Key = 15,
    /// AF_NETLINK 表示Netlink的socket
    Netlink = 16,
    /// AF_PACKET 表示Low level packet interface
    Packet = 17,
    /// AF_ASH 表示Ash
    Ash = 18,
    /// AF_ECONET 表示Acorn Econet
    Econet = 19,
    /// AF_ATMSVC 表示ATM SVCs
    Atmsvc = 20,
    /// AF_RDS 表示Reliable Datagram Sockets
    Rds = 21,
    /// AF_SNA 表示Linux SNA Project
    Sna = 22,
    /// AF_IRDA 表示IRDA sockets
    Irda = 23,
    /// AF_PPPOX 表示PPPoX sockets
    Pppox = 24,
    /// AF_WANPIPE 表示WANPIPE API sockets
    WanPipe = 25,
    /// AF_LLC 表示Linux LLC
    Llc = 26,
    /// AF_IB 表示Native InfiniBand address
    /// 介绍：https://access.redhat.com/documentation/en-us/red_hat_enterprise_linux/9/html-single/configuring_infiniband_and_rdma_networks/index#understanding-infiniband-and-rdma_configuring-infiniband-and-rdma-networks
    Ib = 27,
    /// AF_MPLS 表示MPLS
    Mpls = 28,
    /// AF_CAN 表示Controller Area Network
    Can = 29,
    /// AF_TIPC 表示TIPC sockets
    Tipc = 30,
    /// AF_BLUETOOTH 表示Bluetooth sockets
    Bluetooth = 31,
    /// AF_IUCV 表示IUCV sockets
    Iucv = 32,
    /// AF_RXRPC 表示RxRPC sockets
    Rxrpc = 33,
    /// AF_ISDN 表示mISDN sockets
    Isdn = 34,
    /// AF_PHONET 表示Phonet sockets
    Phonet = 35,
    /// AF_IEEE802154 表示IEEE 802.15.4 sockets
    Ieee802154 = 36,
    /// AF_CAIF 表示CAIF sockets
    Caif = 37,
    /// AF_ALG 表示Algorithm sockets
    Alg = 38,
    /// AF_NFC 表示NFC sockets
    Nfc = 39,
    /// AF_VSOCK 表示vSockets
    Vsock = 40,
    /// AF_KCM 表示Kernel Connection Multiplexor
    Kcm = 41,
    /// AF_QIPCRTR 表示Qualcomm IPC Router
    Qipcrtr = 42,
    /// AF_SMC 表示SMC-R sockets.
    /// reserve number for PF_SMC protocol family that reuses AF_INET address family
    Smc = 43,
    /// AF_XDP 表示XDP sockets
    Xdp = 44,
    /// AF_MCTP 表示Management Component Transport Protocol
    Mctp = 45,
    /// AF_MAX 表示最大的地址族
    Max = 46,
}

impl TryFrom<u16> for AddressFamily {
    type Error = system_error::SystemError;
    fn try_from(x: u16) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u16(x).ok_or(system_error::SystemError::EINVAL);
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
