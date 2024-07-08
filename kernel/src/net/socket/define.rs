bitflags! {
    // #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    pub struct SockOp: u32 {
        const SO_DEBUG = 1;
        const SO_REUSEADDR = 2;
        const SO_TYPE = 3;
        const SO_ERROR = 4;
        const SO_DONTROUTE = 5;
        const SO_BROADCAST = 6;
        const SO_SNDBUF = 7;
        const SO_RCVBUF = 8;
        const SO_SNDBUFFORCE = 32;
        const SO_RCVBUFFORCE = 33;
        const SO_KEEPALIVE = 9;
        const SO_OOBINLINE = 10;
        const SO_NO_CHECK = 11;
        const SO_PRIORITY = 12;
        const SO_LINGER = 13;
        const SO_BSDCOMPAT = 14;
        const SO_REUSEPORT = 15;
        const SO_PASSCRED = 16;
        const SO_PEERCRED = 17;
        const SO_RCVLOWAT = 18;
        const SO_SNDLOWAT = 19;
        const SO_RCVTIMEO_OLD = 20;
        const SO_SNDTIMEO_OLD = 21;

        const SO_SECURITY_AUTHENTICATION = 22;
        const SO_SECURITY_ENCRYPTION_TRANSPORT = 23;
        const SO_SECURITY_ENCRYPTION_NETWORK = 24;

        const SO_BINDTODEVICE = 25;

        /// 与SO_GET_FILTER相同
        const SO_ATTACH_FILTER = 26;
        const SO_DETACH_FILTER = 27;

        const SO_PEERNAME = 28;

        const SO_ACCEPTCONN = 30;

        const SO_PEERSEC = 31;
        const SO_PASSSEC = 34;

        const SO_MARK = 36;

        const SO_PROTOCOL = 38;
        const SO_DOMAIN = 39;

        const SO_RXQ_OVFL = 40;

        /// 与SCM_WIFI_STATUS相同
        const SO_WIFI_STATUS = 41;
        const SO_PEEK_OFF = 42;

        /* Instruct lower device to use last 4-bytes of skb data as FCS */
        const SO_NOFCS = 43;

        const SO_LOCK_FILTER = 44;
        const SO_SELECT_ERR_QUEUE = 45;
        const SO_BUSY_POLL = 46;
        const SO_MAX_PACING_RATE = 47;
        const SO_BPF_EXTENSIONS = 48;
        const SO_INCOMING_CPU = 49;
        const SO_ATTACH_BPF = 50;
        // SO_DETACH_BPF = SO_DETACH_FILTER;
        const SO_ATTACH_REUSEPORT_CBPF = 51;
        const SO_ATTACH_REUSEPORT_EBPF = 52;

        const SO_CNX_ADVICE = 53;
        const SCM_TIMESTAMPING_OPT_STATS = 54;
        const SO_MEMINFO = 55;
        const SO_INCOMING_NAPI_ID = 56;
        const SO_COOKIE = 57;
        const SCM_TIMESTAMPING_PKTINFO = 58;
        const SO_PEERGROUPS = 59;
        const SO_ZEROCOPY = 60;
        /// 与SCM_TXTIME相同
        const SO_TXTIME = 61;

        const SO_BINDTOIFINDEX = 62;

        const SO_TIMESTAMP_OLD = 29;
        const SO_TIMESTAMPNS_OLD = 35;
        const SO_TIMESTAMPING_OLD = 37;
        const SO_TIMESTAMP_NEW = 63;
        const SO_TIMESTAMPNS_NEW = 64;
        const SO_TIMESTAMPING_NEW = 65;

        const SO_RCVTIMEO_NEW = 66;
        const SO_SNDTIMEO_NEW = 67;

        const SO_DETACH_REUSEPORT_BPF = 68;

        const SO_PREFER_BUSY_POLL = 69;
        const SO_BUSY_POLL_BUDGET = 70;

        const SO_NETNS_COOKIE = 71;
        const SO_BUF_LOCK = 72;
        const SO_RESERVE_MEM = 73;
        const SO_TXREHASH = 74;
        const SO_RCVMARK = 75;
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
}
