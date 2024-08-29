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


// bitflags::bitflags! {
//     pub struct Level: i32 {
//         const SOL_SOCKET = 1;
//         const IPPROTO_IP = super::ip::Protocol::IP.bits();
//         const IPPROTO_IPV6 = super::ip::Protocol::IPv6.bits();
//         const IPPROTO_TCP = super::ip::Protocol::TCP.bits();
//     }
// }



// bitflags! {
//     /// @brief socket的选项
//     #[derive(Default)]
//     pub struct Options: u32 {
//         /// 是否阻塞
//         const BLOCK = 1 << 0;
//         /// 是否允许广播
//         const BROADCAST = 1 << 1;
//         /// 是否允许多播
//         const MULTICAST = 1 << 2;
//         /// 是否允许重用地址
//         const REUSEADDR = 1 << 3;
//         /// 是否允许重用端口
//         const REUSEPORT = 1 << 4;
//     }
// }
