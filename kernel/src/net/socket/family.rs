/// # AddressFamily
/// Socket address families.
/// ## Reference
/// https://code.dragonos.org.cn/xref/linux-5.19.10/include/linux/socket.h#180
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

impl core::convert::TryFrom<u16> for AddressFamily {
    type Error = system_error::SystemError;
    fn try_from(x: u16) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        // this will return EINVAL but still works, idk why
        return <Self as FromPrimitive>::from_u16(x).ok_or(Self::Error::EINVAL);
    }
}

use crate::net::socket;
use alloc::sync::Arc;

pub trait Family {
    fn socket(
        stype: socket::PSOCK,
        protocol: u32,
    ) -> Result<Arc<socket::Inode>, system_error::SystemError>;
}
