#[allow(dead_code)]
pub mod eth_protocol {
    pub const ETH_P_ALL: u16 = 0x0003;
    pub const ETH_P_IP: u16 = 0x0800;
    pub const ETH_P_ARP: u16 = 0x0806;
    pub const ETH_P_IPV6: u16 = 0x86dd;
}
#[allow(dead_code)]
pub mod packet_option {
    pub const PACKET_ADD_MEMBERSHIP: usize = 1;
    pub const PACKET_DROP_MEMBERSHIP: usize = 2;
    pub const PACKET_STATISTICS: usize = 6;
    pub const PACKET_COPY_THRESH: usize = 7;
    pub const PACKET_AUXDATA: usize = 8;
    pub const PACKET_ORIGDEV: usize = 9;
    pub const PACKET_VERSION: usize = 10;
    pub const PACKET_RESERVE: usize = 12;
    pub const PACKET_VNET_HDR: usize = 15;
    pub const PACKET_TX_TIMESTAMP: usize = 16;
    pub const PACKET_TIMESTAMP: usize = 17;
    pub const PACKET_QDISC_BYPASS: usize = 20;
}
#[allow(dead_code)]
pub mod packet_mreq_type {
    pub const PACKET_MR_MULTICAST: u16 = 0;
    pub const PACKET_MR_PROMISC: u16 = 1;
    pub const PACKET_MR_ALLMULTI: u16 = 2;
    pub const PACKET_MR_UNICAST: u16 = 3;
}
#[repr(C)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct PacketMreq {
    pub mr_ifindex: i32,
    pub mr_type: u16,
    pub mr_alen: u16,
    pub mr_address: [u8; 8],
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PacketType {
    #[default]
    Host = 0,
    Broadcast = 1,
    Multicast = 2,
    OtherHost = 3,
    Outgoing = 4,
    Loopback = 5,
}
#[derive(Debug, Clone, Default)]
#[repr(C)]
pub struct SockAddrLl {
    pub sll_family: u16,
    pub sll_protocol: u16,
    pub sll_ifindex: i32,
    pub sll_hatype: u16,
    pub sll_pkttype: u8,
    pub sll_halen: u8,
    pub sll_addr: [u8; 8],
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TpacketAuxdata {
    pub tp_status: u32,
    pub tp_len: u32,
    pub tp_snaplen: u32,
    pub tp_mac: u16,
    pub tp_net: u16,
    pub tp_vlan_tci: u16,
    pub tp_vlan_tpid: u16,
}
pub const SOL_PACKET: i32 = 263;
pub const TP_STATUS_USER: u32 = 1;
pub const TP_STATUS_VLAN_VALID: u32 = 1 << 4;
pub const TP_STATUS_VLAN_TPID_VALID: u32 = 1 << 6;
