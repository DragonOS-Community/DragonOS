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
    pub const PACKET_RECV_OUTPUT: usize = 3;
    pub const PACKET_RX_RING: usize = 5;
    pub const PACKET_STATISTICS: usize = 6;
    pub const PACKET_COPY_THRESH: usize = 7;
    pub const PACKET_AUXDATA: usize = 8;
    pub const PACKET_ORIGDEV: usize = 9;
    pub const PACKET_VERSION: usize = 10;
    pub const PACKET_HDRLEN: usize = 11;
    pub const PACKET_RESERVE: usize = 12;
    pub const PACKET_TX_RING: usize = 13;
    pub const PACKET_LOSS: usize = 14;
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

// ---------------------------------------------------------------------------
// TPACKET ring buffer UAPI (PACKET_RX_RING / PACKET_TX_RING)
// Layout matches Linux include/uapi/linux/if_packet.h (Linux 6.6 semantics).
// ---------------------------------------------------------------------------

/// TPACKET ring versions (enum tpacket_versions).
#[allow(dead_code)]
pub mod tpacket_version {
    pub const TPACKET_V1: i32 = 0;
    pub const TPACKET_V2: i32 = 1;
    pub const TPACKET_V3: i32 = 2;
}

/// Frame alignment for all TPACKET versions.
pub const TPACKET_ALIGNMENT: usize = 16;
/// Align `x` up to [`TPACKET_ALIGNMENT`].
pub const fn tpacket_align(x: usize) -> usize {
    (x + TPACKET_ALIGNMENT - 1) & !(TPACKET_ALIGNMENT - 1)
}

// --- RX ring header status flags -------------------------------------------
pub const TP_STATUS_KERNEL: u32 = 0;
// TP_STATUS_USER (1) already defined above.

/// V1 frame header (`struct tpacket_hdr`). 28 bytes on x86_64.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TpacketHdr {
    pub tp_status: u64,
    pub tp_len: u32,
    pub tp_snaplen: u32,
    pub tp_mac: u16,
    pub tp_net: u16,
    pub tp_sec: u32,
    pub tp_usec: u32,
}

/// V2 frame header (`struct tpacket2_hdr`). 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Tpacket2Hdr {
    pub tp_status: u32,
    pub tp_len: u32,
    pub tp_snaplen: u32,
    pub tp_mac: u16,
    pub tp_net: u16,
    pub tp_sec: u32,
    pub tp_nsec: u32,
    pub tp_vlan_tci: u16,
    pub tp_vlan_tpid: u16,
    pub tp_padding: [u8; 4],
}

/// `struct tpacket_req` — ring configuration for V1/V2.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct TpacketReq {
    pub tp_block_size: u32,
    pub tp_block_nr: u32,
    pub tp_frame_size: u32,
    pub tp_frame_nr: u32,
}

/// `struct tpacket_stats` — returned by PACKET_STATISTICS (V1/V2).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct TpacketStats {
    pub tp_packets: u32,
    pub tp_drops: u32,
}

/// Header region size per version (= `TPACKET_ALIGN(sizeof(hdr)) + sizeof(sockaddr_ll)`).
/// sockaddr_ll = 20 bytes. V1: align(28)+20 = 52. V2: align(32)+20 = 52.
pub const TPACKET_HDRLEN: usize = tpacket_align(28) + 20;
pub const TPACKET2_HDRLEN: usize = tpacket_align(32) + 20;
