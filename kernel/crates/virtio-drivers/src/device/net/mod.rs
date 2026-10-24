//! Driver for VirtIO network devices.

#[cfg(feature = "alloc")]
mod dev;
mod dev_raw;
#[cfg(feature = "alloc")]
mod net_buf;

pub use self::dev_raw::VirtIONetRaw;
#[cfg(feature = "alloc")]
pub use self::{dev::VirtIONet, net_buf::RxBuffer, net_buf::TxBuffer};

use crate::volatile::ReadOnly;
use bitflags::bitflags;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

const MAX_BUFFER_LEN: usize = 65535;
const MIN_BUFFER_LEN: usize = 1526;
const NET_HDR_SIZE: usize = core::mem::size_of::<VirtioNetHdr>();
const MODERN_NET_HDR_SIZE: usize = NET_HDR_SIZE + 2;

/// Minimum wire capacity when TCP receive segmentation is negotiated: a
/// non-jumbo IPv6 packet, Ethernet with one VLAN tag, and the modern header.
/// Allocation/page rounding is the caller's policy.
pub const MIN_GSO_RX_BUFFER_LEN: usize = 65535 + 40 + 18 + MODERN_NET_HDR_SIZE;

bitflags! {
    /// Receive offloads the caller can actually consume through metadata.
    /// The default raw, buffered and blocking packet APIs do not enable these.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct RxOffloads: u8 {
        /// TCP segmentation over IPv4.
        const TCPV4 = 1;
        /// TCP segmentation over IPv6.
        const TCPV6 = 2;
        /// ECN TCP segmentation, in combination with TCPV4 or TCPV6.
        const ECN = 4;
    }
}

impl RxOffloads {
    fn features(self) -> Features {
        let mut features = Features::empty();
        if self.contains(Self::TCPV4) {
            features |= Features::GUEST_TSO4;
        }
        if self.contains(Self::TCPV6) {
            features |= Features::GUEST_TSO6;
        }
        if self.contains(Self::ECN) {
            features |= Features::GUEST_ECN;
        }
        features
    }

    fn negotiated(features: Features) -> Self {
        let mut offloads = Self::empty();
        if features.contains(Features::GUEST_TSO4) {
            offloads |= Self::TCPV4;
        }
        if features.contains(Features::GUEST_TSO6) {
            offloads |= Self::TCPV6;
        }
        if features.contains(Features::GUEST_ECN) {
            offloads |= Self::ECN;
        }
        offloads
    }
}

/// The checksum location relative to the Ethernet frame, not the virtio header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartialChecksum {
    /// First byte included in the checksum, relative to Ethernet frame start.
    pub start: u16,
    /// Checksum field displacement from start.
    pub offset: u16,
}

/// Supported TCP receive segmentation formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpGsoKind {
    /// TCP over IPv4.
    TcpV4,
    /// TCP over IPv6.
    TcpV6,
}

/// A CPU-owned aggregate. The caller must segment it before normal ingress.
/// The checksum field still contains the original seed when checksum is Some.
/// IP/TCP structure and checksums remain the consumer's responsibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpGsoMetadata {
    /// Network protocol carrying the TCP aggregate.
    pub kind: TcpGsoKind,
    /// Whether the device requested ECN-aware segmentation.
    pub ecn: bool,
    /// Maximum TCP payload bytes per segment, excluding protocol headers.
    pub gso_size: u16,
    /// Partial checksum location; None means a complete aggregate checksum.
    pub checksum: Option<PartialChecksum>,
}

/// Additional processing required before the frame can enter a normal stack.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReceiveMetadata {
    /// Segmentation required before delivering the packet to ordinary ingress.
    pub gso: Option<TcpGsoMetadata>,
}

/// Immutable negotiated wire format. Copy it while holding the driver lock,
/// then validate/complete a reclaimed CPU-owned frame outside that lock.
#[derive(Clone, Copy, Debug)]
pub struct ReceiveParser {
    layout: HeaderLayout,
    guest_csum: bool,
    offloads: RxOffloads,
}

bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct Features: u64 {
        /// Device handles packets with partial checksum.
        /// This "checksum offload" is a common feature on modern network cards.
        const CSUM = 1 << 0;
        /// Driver handles packets with partial checksum.
        const GUEST_CSUM = 1 << 1;
        /// Control channel offloads reconfiguration support.
        const CTRL_GUEST_OFFLOADS = 1 << 2;
        /// Device maximum MTU reporting is supported.
        ///
        /// If offered by the device, device advises driver about the value of
        /// its maximum MTU. If negotiated, the driver uses mtu as the maximum
        /// MTU value.
        const MTU = 1 << 3;
        /// Device has given MAC address.
        const MAC = 1 << 5;
        /// Device handles packets with any GSO type. (legacy)
        const GSO = 1 << 6;
        /// Driver can receive TSOv4.
        const GUEST_TSO4 = 1 << 7;
        /// Driver can receive TSOv6.
        const GUEST_TSO6 = 1 << 8;
        /// Driver can receive TSO with ECN.
        const GUEST_ECN = 1 << 9;
        /// Driver can receive UFO.
        const GUEST_UFO = 1 << 10;
        /// Device can receive TSOv4.
        const HOST_TSO4 = 1 << 11;
        /// Device can receive TSOv6.
        const HOST_TSO6 = 1 << 12;
        /// Device can receive TSO with ECN.
        const HOST_ECN = 1 << 13;
        /// Device can receive UFO.
        const HOST_UFO = 1 << 14;
        /// Driver can merge receive buffers.
        const MRG_RXBUF = 1 << 15;
        /// Configuration status field is available.
        const STATUS = 1 << 16;
        /// Control channel is available.
        const CTRL_VQ = 1 << 17;
        /// Control channel RX mode support.
        const CTRL_RX = 1 << 18;
        /// Control channel VLAN filtering.
        const CTRL_VLAN = 1 << 19;
        ///
        const CTRL_RX_EXTRA = 1 << 20;
        /// Driver can send gratuitous packets.
        const GUEST_ANNOUNCE = 1 << 21;
        /// Device supports multiqueue with automatic receive steering.
        const MQ = 1 << 22;
        /// Set MAC address through control channel.
        const CTL_MAC_ADDR = 1 << 23;

        // device independent
        const RING_INDIRECT_DESC = 1 << 28;
        const RING_EVENT_IDX = 1 << 29;
        const VERSION_1 = 1 << 32;
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct Status: u16 {
        const LINK_UP = 1;
        const ANNOUNCE = 2;
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct InterruptStatus : u32 {
        const USED_RING_UPDATE = 1 << 0;
        const CONFIGURATION_CHANGE = 1 << 1;
    }
}

#[repr(C)]
struct Config {
    mac: ReadOnly<EthernetAddress>,
    status: ReadOnly<Status>,
    max_virtqueue_pairs: ReadOnly<u16>,
    mtu: ReadOnly<u16>,
}

type EthernetAddress = [u8; 6];

/// VirtIO 5.1.6 Device Operation:
///
/// Packets are transmitted by placing them in the transmitq1. . .transmitqN,
/// and buffers for incoming packets are placed in the receiveq1. . .receiveqN.
/// In each case, the packet itself is preceded by a header.
#[repr(C)]
#[derive(AsBytes, Debug, Default, FromBytes, FromZeroes)]
pub struct VirtioNetHdr {
    flags: Flags,
    gso_type: GsoType,
    hdr_len: u16, // cannot rely on this
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    // The modern wire format appends num_buffers even without MRG_RXBUF.
    // payload starts from here
}

#[derive(AsBytes, Copy, Clone, Debug, Default, Eq, FromBytes, FromZeroes, PartialEq)]
#[repr(transparent)]
struct Flags(u8);

bitflags! {
    impl Flags: u8 {
        const NEEDS_CSUM = 1;
        const DATA_VALID = 2;
        const RSC_INFO   = 4;
    }
}

#[repr(transparent)]
#[derive(AsBytes, Debug, Copy, Clone, Default, Eq, FromBytes, FromZeroes, PartialEq)]
struct GsoType(u8);

impl GsoType {
    const NONE: GsoType = GsoType(0);
    const TCPV4: GsoType = GsoType(1);
    const UDP: GsoType = GsoType(3);
    const TCPV6: GsoType = GsoType(4);
    const ECN: GsoType = GsoType(0x80);
}

const QUEUE_RECEIVE: u16 = 0;
const QUEUE_TRANSMIT: u16 = 1;
const SUPPORTED_FEATURES: Features = Features::MAC
    .union(Features::GUEST_CSUM)
    .union(Features::STATUS)
    .union(Features::RING_EVENT_IDX)
    .union(Features::VERSION_1);

/// The common prefix remains ten bytes even when the v1 wire header has
/// num_buffers appended. Without MRG_RXBUF that field is ignored on receive.
#[derive(Clone, Copy, Debug)]
enum HeaderLayout {
    Legacy,
    Modern,
}

impl HeaderLayout {
    fn len(self) -> usize {
        match self {
            Self::Legacy => NET_HDR_SIZE,
            Self::Modern => MODERN_NET_HDR_SIZE,
        }
    }

    fn write(self, buffer: &mut [u8]) -> crate::Result<usize> {
        let header = buffer
            .get_mut(..self.len())
            .ok_or(crate::Error::InvalidParam)?;
        header.fill(0);
        Ok(self.len())
    }

    fn receive(
        self,
        buffer: &mut [u8],
        used: usize,
        guest_csum: bool,
    ) -> crate::Result<(usize, usize)> {
        ReceiveParser {
            layout: self,
            guest_csum,
            offloads: RxOffloads::empty(),
        }
        .prepare(buffer, used)
        .map(|(header, packet, _)| (header, packet))
    }

    fn read_u16(self, bytes: [u8; 2]) -> u16 {
        match self {
            Self::Legacy => u16::from_ne_bytes(bytes),
            Self::Modern => u16::from_le_bytes(bytes),
        }
    }
}

impl ReceiveParser {
    /// Validate a reclaimed receive buffer and return its payload boundaries.
    /// Ordinary packets have their partial checksum completed. TCP aggregates
    /// retain their original checksum/seed for the segmenter and require an
    /// explicitly negotiated receive offload. This method performs no DMA I/O;
    /// ordinary partial-checksum completion may scan the entire packet.
    pub fn prepare(
        self,
        buffer: &mut [u8],
        used: usize,
    ) -> crate::Result<(usize, usize, ReceiveMetadata)> {
        let header = self.layout.len();
        if used < header || used > buffer.len() {
            return Err(crate::Error::IoError);
        }
        if buffer[0] & Flags::RSC_INFO.bits() != 0 {
            return Err(crate::Error::IoError);
        }
        let gso = match buffer[1] {
            0 => None,
            raw => {
                let ecn = raw & GsoType::ECN.0 != 0;
                if ecn && !self.offloads.contains(RxOffloads::ECN) {
                    return Err(crate::Error::IoError);
                }
                let kind = match raw & !GsoType::ECN.0 {
                    1 if self.offloads.contains(RxOffloads::TCPV4) => TcpGsoKind::TcpV4,
                    4 if self.offloads.contains(RxOffloads::TCPV6) => TcpGsoKind::TcpV6,
                    _ => return Err(crate::Error::IoError),
                };
                let gso_size = self.layout.read_u16([buffer[4], buffer[5]]);
                if gso_size == 0 || gso_size == u16::MAX {
                    return Err(crate::Error::IoError);
                }
                Some(TcpGsoMetadata {
                    kind,
                    ecn,
                    gso_size,
                    checksum: None,
                })
            }
        };
        let checksum = if buffer[0] & Flags::NEEDS_CSUM.bits() != 0 {
            if !self.guest_csum {
                return Err(crate::Error::IoError);
            }
            let start = self.layout.read_u16([buffer[6], buffer[7]]);
            let offset = self.layout.read_u16([buffer[8], buffer[9]]);
            let end = (start as usize)
                .checked_add(offset as usize)
                .and_then(|field| field.checked_add(2))
                .ok_or(crate::Error::IoError)?;
            if end > used - header {
                return Err(crate::Error::IoError);
            }
            Some(PartialChecksum { start, offset })
        } else {
            None
        };
        if let Some(mut gso) = gso {
            // Do not complete an aggregate checksum or change its seed. The
            // consumer must use the original value when forming each segment.
            gso.checksum = checksum;
            return Ok((header, used - header, ReceiveMetadata { gso: Some(gso) }));
        }
        if let Some(checksum) = checksum {
            let packet = &mut buffer[header..used];
            let field = checksum.start as usize + checksum.offset as usize;
            // Match Linux skb_checksum_help: keep the pseudo-header seed in
            // the sum, and complete only after the descriptor is CPU-owned.
            let mut words = packet[checksum.start as usize..].chunks_exact(2);
            let mut sum: u64 = words
                .by_ref()
                .map(|word| u16::from_be_bytes([word[0], word[1]]) as u64)
                .sum();
            if let Some(&last) = words.remainder().first() {
                sum += (last as u64) << 8;
            }
            while sum >> 16 != 0 {
                sum = (sum & 0xffff) + (sum >> 16);
            }
            let checksum = !(sum as u16);
            let checksum = if checksum == 0 { u16::MAX } else { checksum };
            packet[field..field + 2].copy_from_slice(&checksum.to_be_bytes());
            buffer[0] &= !Flags::NEEDS_CSUM.bits();
        }
        Ok((header, used - header, ReceiveMetadata::default()))
    }
}

#[cfg(test)]
mod tests;
