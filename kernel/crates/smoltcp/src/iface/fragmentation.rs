#![allow(unused)]

use core::fmt;

use managed::{ManagedMap, ManagedSlice};

use crate::config::{FRAGMENTATION_BUFFER_SIZE, REASSEMBLY_BUFFER_COUNT, REASSEMBLY_BUFFER_SIZE};
use crate::storage::Assembler;
use crate::time::{Duration, Instant};
use crate::wire::*;

use core::result::Result;

#[cfg(feature = "alloc")]
type Buffer = crate::storage::PacketBytes;
#[cfg(not(feature = "alloc"))]
type Buffer = [u8; REASSEMBLY_BUFFER_SIZE];

#[cfg(feature = "alloc")]
type FragmentationBuffer = crate::storage::PacketBytes;
#[cfg(not(feature = "alloc"))]
type FragmentationBuffer = [u8; FRAGMENTATION_BUFFER_SIZE];

/// Problem when assembling: something was out of bounds.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct AssemblerError;

impl fmt::Display for AssemblerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "AssemblerError")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for AssemblerError {}

/// Packet assembler is full
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct AssemblerFullError;

#[cfg(feature = "proto-ipv4-fragmentation")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FragmentRange {
    New,
    Duplicate,
    Overlap,
}

impl fmt::Display for AssemblerFullError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "AssemblerFullError")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for AssemblerFullError {}

/// Holds different fragments of one packet, used for assembling fragmented packets.
///
/// The buffer used for the `PacketAssembler` should either be dynamically sized (ex: Vec<u8>)
/// or should be statically allocated based upon the MTU of the type of packet being
/// assembled (ex: 1280 for a IPv6 frame).
#[derive(Debug)]
pub struct PacketAssembler<K> {
    key: Option<K>,
    buffer: Buffer,
    // IPv4 reassembly must retain the offset-zero header, including options:
    // the fragment completing a datagram need not be the first fragment.
    #[cfg(feature = "proto-ipv4-fragmentation")]
    first_ipv4_header: Option<([u8; 60], u8, HardwareAddress, u32)>,

    assembler: Assembler,
    total_size: Option<usize>,
    expires_at: Instant,
}

impl<K> PacketAssembler<K> {
    /// Create a new empty buffer for fragments.
    pub const fn new() -> Self {
        Self {
            key: None,

            #[cfg(feature = "alloc")]
            buffer: Buffer::new(),
            #[cfg(not(feature = "alloc"))]
            buffer: [0u8; REASSEMBLY_BUFFER_SIZE],
            #[cfg(feature = "proto-ipv4-fragmentation")]
            first_ipv4_header: None,

            assembler: Assembler::new(),
            total_size: None,
            expires_at: Instant::ZERO,
        }
    }

    pub fn reset(&mut self) {
        self.key = None;
        self.assembler.clear();
        self.total_size = None;
        self.expires_at = Instant::ZERO;
        #[cfg(feature = "proto-ipv4-fragmentation")]
        {
            self.first_ipv4_header = None;
        }
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(crate) fn set_first_ipv4_header(
        &mut self,
        header: &[u8],
        source_hardware_addr: HardwareAddress,
        mark: u32,
    ) {
        if self.first_ipv4_header.is_none() {
            debug_assert!((20..=60).contains(&header.len()));
            let mut saved = [0; 60];
            saved[..header.len()].copy_from_slice(header);
            self.first_ipv4_header = Some((saved, header.len() as u8, source_hardware_addr, mark));
        }
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(crate) fn first_ipv4_header(&self) -> Option<&[u8]> {
        self.first_ipv4_header
            .as_ref()
            .map(|(header, len, _, _)| &header[..*len as usize])
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(crate) fn first_ipv4_source_hardware_addr(&self) -> Option<HardwareAddress> {
        self.first_ipv4_header
            .as_ref()
            .map(|(_, _, source, _)| *source)
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(crate) fn first_ipv4_mark(&self) -> Option<u32> {
        self.first_ipv4_header.as_ref().map(|(_, _, _, mark)| *mark)
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(crate) fn classify_received_range(&self, start: usize, end: usize) -> FragmentRange {
        let mut overlap = false;
        for (received_start, received_end) in self.assembler.iter_data(0) {
            if start >= received_start && end <= received_end {
                return FragmentRange::Duplicate;
            }
            overlap |= start < received_end && received_start < end;
        }
        if overlap {
            FragmentRange::Overlap
        } else {
            FragmentRange::New
        }
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(crate) fn total_size(&self) -> Option<usize> {
        self.total_size
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(crate) fn last_received_end(&self) -> usize {
        self.assembler.iter_data(0).last().map_or(0, |(_, end)| end)
    }

    #[cfg(feature = "alloc")]
    fn ensure_buffer_len(&mut self, size: usize) -> Result<(), AssemblerError> {
        if self.buffer.len() >= size {
            return Ok(());
        }
        self.buffer
            .try_reserve_exact(size - self.buffer.len())
            .map_err(|_| AssemblerError)?;
        self.buffer.resize(size, 0);
        Ok(())
    }

    fn release_oversized_buffer(&mut self) {
        #[cfg(feature = "alloc")]
        self.buffer.release_owned();
        #[cfg(feature = "alloc")]
        if self.buffer.capacity() > REASSEMBLY_BUFFER_SIZE {
            drop(self.buffer.take());
        }
    }

    /// Set the total size of the packet assembler.
    pub fn set_total_size(&mut self, size: usize) -> Result<(), AssemblerError> {
        if let Some(old_size) = self.total_size {
            if old_size != size {
                return Err(AssemblerError);
            }
        }

        #[cfg(not(feature = "alloc"))]
        if self.buffer.len() < size {
            return Err(AssemblerError);
        }

        #[cfg(feature = "alloc")]
        self.ensure_buffer_len(size)?;

        self.total_size = Some(size);
        Ok(())
    }

    /// Return the instant when the assembler expires.
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub fn add_with(
        &mut self,
        offset: usize,
        f: impl Fn(&mut [u8]) -> Result<usize, AssemblerError>,
    ) -> Result<(), AssemblerError> {
        if self.buffer.len() < offset {
            return Err(AssemblerError);
        }

        let len = f(&mut self.buffer[offset..])?;
        assert!(offset + len <= self.buffer.len());

        net_debug!(
            "frag assembler: receiving {} octets at offset {}",
            len,
            offset
        );

        self.assembler
            .add(offset, len)
            .map_err(|_| AssemblerError)?;
        Ok(())
    }

    /// Add a fragment into the packet that is being reassembled.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::PacketAssemblerBufferTooSmall`] when trying to add data into the buffer at a non-existing
    ///   place.
    pub fn add(&mut self, data: &[u8], offset: usize) -> Result<(), AssemblerError> {
        let end = offset.checked_add(data.len()).ok_or(AssemblerError)?;

        #[cfg(not(feature = "alloc"))]
        if self.buffer.len() < end {
            return Err(AssemblerError);
        }

        #[cfg(feature = "alloc")]
        self.ensure_buffer_len(end)?;

        let len = data.len();
        self.assembler
            .add(offset, len)
            .map_err(|_| AssemblerError)?;
        self.buffer[offset..][..len].copy_from_slice(data);

        net_debug!(
            "frag assembler: receiving {} octets at offset {}",
            len,
            offset
        );

        Ok(())
    }

    /// Get an immutable slice of the underlying packet data, if reassembly complete.
    /// This will mark the assembler as empty, so that it can be reused.
    pub fn assemble(&mut self) -> Option<&'_ [u8]> {
        if !self.is_complete() {
            return None;
        }

        // NOTE: we can unwrap because `is_complete` already checks this.
        let total_size = self.total_size.unwrap();
        self.reset();
        Some(&self.buffer[..total_size])
    }

    /// Returns `true` when all fragments have been received, otherwise `false`.
    pub fn is_complete(&self) -> bool {
        self.total_size == Some(self.assembler.peek_front())
    }

    /// Returns `true` when the packet assembler is free to use.
    fn is_free(&self) -> bool {
        self.key.is_none()
    }
}

/// Set holding multiple [`PacketAssembler`].
#[derive(Debug)]
pub struct PacketAssemblerSet<K: Eq + Copy> {
    assemblers: [PacketAssembler<K>; REASSEMBLY_BUFFER_COUNT],
}

impl<K: Eq + Copy> PacketAssemblerSet<K> {
    const NEW_PA: PacketAssembler<K> = PacketAssembler::new();

    /// Create a new set of packet assemblers.
    pub fn new() -> Self {
        Self {
            assemblers: [Self::NEW_PA; REASSEMBLY_BUFFER_COUNT],
        }
    }

    #[cfg(feature = "alloc")]
    pub(crate) fn set_allocator(
        &mut self,
        allocator: crate::storage::PacketBytesAllocator,
    ) -> Result<(), crate::storage::OwnedBufferResizeError> {
        if self.assemblers.iter().any(|a| !a.is_free()) {
            return Err(crate::storage::OwnedBufferResizeError);
        }
        for assembler in &mut self.assemblers {
            assembler.buffer = crate::storage::PacketBytes::with_allocator(allocator);
        }
        Ok(())
    }

    /// Get a [`PacketAssembler`] for a specific key.
    ///
    /// If it doesn't exist, it is created, with the `expires_at` timestamp.
    ///
    /// If the assembler set is full, in which case an error is returned.
    pub fn get(
        &mut self,
        key: &K,
        expires_at: Instant,
    ) -> Result<&mut PacketAssembler<K>, AssemblerFullError> {
        let mut empty_slot = None;
        for slot in &mut self.assemblers {
            if slot.key.as_ref() == Some(key) {
                return Ok(slot);
            }
            if slot.is_free() {
                empty_slot = Some(slot)
            }
        }

        let slot = empty_slot.ok_or(AssemblerFullError)?;
        slot.key = Some(*key);
        slot.expires_at = expires_at;
        Ok(slot)
    }

    /// Remove all [`PacketAssembler`]s that are expired.
    pub fn remove_expired(&mut self, timestamp: Instant) {
        for frag in &mut self.assemblers {
            if !frag.is_free() && frag.expires_at < timestamp {
                frag.reset();
            }
            if frag.is_free() {
                frag.release_oversized_buffer();
            }
        }
    }
}

// Max len of non-fragmented packets after decompression (including ipv6 header and payload)
// TODO: lower. Should be (6lowpan mtu) - (min 6lowpan header size) + (max ipv6 header size)
pub const MAX_DECOMPRESSED_LEN: usize = 1500;

#[cfg(feature = "_proto-fragmentation")]
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum FragKey {
    #[cfg(feature = "proto-ipv4-fragmentation")]
    Ipv4(Ipv4FragKey),
    #[cfg(feature = "proto-sixlowpan-fragmentation")]
    Sixlowpan(SixlowpanFragKey),
}

pub struct FragmentsBuffer {
    #[cfg(feature = "proto-sixlowpan")]
    pub decompress_buf: [u8; MAX_DECOMPRESSED_LEN],

    #[cfg(feature = "_proto-fragmentation")]
    pub assembler: PacketAssemblerSet<FragKey>,

    #[cfg(feature = "_proto-fragmentation")]
    pub reassembly_timeout: Duration,
}

#[cfg(not(feature = "_proto-fragmentation"))]
pub struct Fragmenter {}

#[cfg(not(feature = "_proto-fragmentation"))]
impl Fragmenter {
    pub fn new() -> Self {
        Self {}
    }
}

#[cfg(feature = "_proto-fragmentation")]
pub struct Fragmenter {
    /// The buffer that holds the unfragmented 6LoWPAN packet.
    pub buffer: FragmentationBuffer,
    /// The size of the packet without the IEEE802.15.4 header and the fragmentation headers.
    pub packet_len: usize,
    /// The amount of bytes that already have been transmitted.
    pub sent_bytes: usize,

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub ipv4: Ipv4Fragmenter,
    #[cfg(feature = "proto-sixlowpan-fragmentation")]
    pub sixlowpan: SixlowpanFragmenter,
}

#[cfg(feature = "proto-ipv4-fragmentation")]
pub struct Ipv4Fragmenter {
    /// The IPv4 representation.
    pub repr: Ipv4Repr,
    /// Packet metadata applied to every fragment.
    pub meta: crate::phy::PacketMeta,
    /// Route-selected serialization properties shared by every fragment.
    pub egress: Option<crate::phy::TxEgressOverride>,
    /// The destination hardware address.
    #[cfg(feature = "medium-ethernet")]
    pub dst_hardware_addr: EthernetAddress,
    /// The offset of the next fragment.
    pub frag_offset: u16,
    /// The identifier of the stream.
    pub ident: u16,
}

#[cfg(feature = "proto-sixlowpan-fragmentation")]
pub struct SixlowpanFragmenter {
    /// The datagram size that is used for the fragmentation headers.
    pub datagram_size: u16,
    /// The datagram tag that is used for the fragmentation headers.
    pub datagram_tag: u16,
    pub datagram_offset: usize,

    /// The size of the FRAG_N packets.
    pub fragn_size: usize,

    /// The link layer IEEE802.15.4 source address.
    pub ll_dst_addr: Ieee802154Address,
    /// The link layer IEEE802.15.4 source address.
    pub ll_src_addr: Ieee802154Address,
}

#[cfg(feature = "_proto-fragmentation")]
impl Fragmenter {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "alloc")]
            buffer: crate::storage::PacketBytes::new(),
            #[cfg(not(feature = "alloc"))]
            buffer: [0u8; FRAGMENTATION_BUFFER_SIZE],
            packet_len: 0,
            sent_bytes: 0,

            #[cfg(feature = "proto-ipv4-fragmentation")]
            ipv4: Ipv4Fragmenter {
                repr: Ipv4Repr {
                    src_addr: Ipv4Address::new(0, 0, 0, 0),
                    dst_addr: Ipv4Address::new(0, 0, 0, 0),
                    next_header: IpProtocol::Unknown(0),
                    payload_len: 0,
                    hop_limit: 0,
                },
                meta: crate::phy::PacketMeta::default(),
                egress: None,
                #[cfg(feature = "medium-ethernet")]
                dst_hardware_addr: EthernetAddress::default(),
                frag_offset: 0,
                ident: 0,
            },

            #[cfg(feature = "proto-sixlowpan-fragmentation")]
            sixlowpan: SixlowpanFragmenter {
                datagram_size: 0,
                datagram_tag: 0,
                datagram_offset: 0,
                fragn_size: 0,
                ll_dst_addr: Ieee802154Address::Absent,
                ll_src_addr: Ieee802154Address::Absent,
            },
        }
    }

    /// Return `true` when everything is transmitted.
    #[inline]
    pub fn finished(&self) -> bool {
        self.packet_len == self.sent_bytes
    }

    /// Returns `true` when there is nothing to transmit.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.packet_len == 0
    }

    /// Prepare storage for one complete packet before fragmentation starts.
    /// Allocating integrations grow on demand; allocation-free integrations
    /// retain their configured fixed-capacity behavior.
    pub fn prepare_buffer(&mut self, len: usize) -> bool {
        #[cfg(feature = "alloc")]
        {
            if self.buffer.len() < len {
                if self
                    .buffer
                    .try_reserve_exact(len - self.buffer.len())
                    .is_err()
                {
                    return false;
                }
                self.buffer.resize(len, 0);
            }
            true
        }
        #[cfg(not(feature = "alloc"))]
        {
            self.buffer.len() >= len
        }
    }

    // Reset the buffer.
    pub fn reset(&mut self) {
        self.packet_len = 0;
        self.sent_bytes = 0;
        #[cfg(feature = "alloc")]
        self.buffer.release_owned();

        #[cfg(feature = "proto-ipv4-fragmentation")]
        {
            self.ipv4.repr = Ipv4Repr {
                src_addr: Ipv4Address::new(0, 0, 0, 0),
                dst_addr: Ipv4Address::new(0, 0, 0, 0),
                next_header: IpProtocol::Unknown(0),
                payload_len: 0,
                hop_limit: 0,
            };
            self.ipv4.meta = crate::phy::PacketMeta::default();
            self.ipv4.egress = None;
            #[cfg(feature = "medium-ethernet")]
            {
                self.ipv4.dst_hardware_addr = EthernetAddress::default();
            }
        }

        #[cfg(feature = "proto-sixlowpan-fragmentation")]
        {
            self.sixlowpan.datagram_size = 0;
            self.sixlowpan.datagram_tag = 0;
            self.sixlowpan.fragn_size = 0;
            self.sixlowpan.ll_dst_addr = Ieee802154Address::Absent;
            self.sixlowpan.ll_src_addr = Ieee802154Address::Absent;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
    struct Key {
        id: usize,
    }

    #[test]
    fn packet_assembler_overlap() {
        let mut p_assembler = PacketAssembler::<Key>::new();

        p_assembler.set_total_size(5).unwrap();

        let data = b"Rust";
        p_assembler.add(&data[..], 0);
        p_assembler.add(&data[..], 1);

        assert_eq!(p_assembler.assemble(), Some(&b"RRust"[..]))
    }

    #[test]
    fn packet_assembler_rejects_offset_overflow() {
        let mut p_assembler = PacketAssembler::<Key>::new();
        assert_eq!(p_assembler.add(&[1], usize::MAX), Err(AssemblerError));
    }

    #[test]
    #[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ethernet"))]
    fn packet_assembler_retains_and_clears_first_ipv4_header() {
        let mut assembler = PacketAssembler::<Key>::new();
        let source_mac =
            HardwareAddress::Ethernet(crate::wire::EthernetAddress([2, 0, 0, 0, 0, 1]));
        let mut first_header = [0u8; 24];
        first_header[0] = 0x46;
        first_header[8] = 64;
        first_header[20..].copy_from_slice(&[1, 2, 3, 4]);
        assembler.set_first_ipv4_header(&first_header, source_mac, 0x1234);
        assembler.set_first_ipv4_header(&[9; 20], source_mac, 0xabcd);
        assert_eq!(assembler.first_ipv4_header(), Some(&first_header[..]));
        assert_eq!(assembler.first_ipv4_mark(), Some(0x1234));
        assert_eq!(
            assembler.first_ipv4_source_hardware_addr(),
            Some(source_mac)
        );

        assembler.set_total_size(1).unwrap();
        assembler.add(&[7], 0).unwrap();
        assert_eq!(assembler.assemble(), Some(&[7][..]));
        assert_eq!(assembler.first_ipv4_header(), None);
        assert_eq!(assembler.first_ipv4_source_hardware_addr(), None);
        assert_eq!(assembler.first_ipv4_mark(), None);
    }

    #[test]
    #[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ethernet"))]
    fn packet_assembler_keeps_offset_zero_fragment_source_mac() {
        let mut assembler = PacketAssembler::<Key>::new();
        let first_mac = HardwareAddress::Ethernet(EthernetAddress([2, 0, 0, 0, 0, 1]));
        let later_mac = HardwareAddress::Ethernet(EthernetAddress([2, 0, 0, 0, 0, 2]));
        assembler.set_first_ipv4_header(&[0x45; 20], first_mac, 7);
        assembler.set_first_ipv4_header(&[0x45; 20], later_mac, 8);
        assert_eq!(assembler.first_ipv4_source_hardware_addr(), Some(first_mac));
        assert_eq!(assembler.first_ipv4_mark(), Some(7));
        assembler.reset();
        assert_eq!(assembler.first_ipv4_source_hardware_addr(), None);
        assert_eq!(assembler.first_ipv4_mark(), None);
    }

    #[test]
    #[cfg(feature = "proto-ipv4-fragmentation")]
    fn packet_assembler_detects_partial_and_complete_overlap() {
        let mut assembler = PacketAssembler::<Key>::new();
        assembler.add(&[1; 8], 0).unwrap();
        assembler.add(&[2; 8], 16).unwrap();
        assert_eq!(assembler.classify_received_range(8, 16), FragmentRange::New);
        assert_eq!(
            assembler.classify_received_range(0, 8),
            FragmentRange::Duplicate
        );
        assert_eq!(
            assembler.classify_received_range(4, 12),
            FragmentRange::Overlap
        );
        assert_eq!(
            assembler.classify_received_range(20, 28),
            FragmentRange::Overlap
        );
        assert_eq!(
            assembler.classify_received_range(24, 32),
            FragmentRange::New
        );
    }

    #[test]
    fn packet_assembler_reports_full_range_index() {
        let mut assembler = PacketAssembler::<Key>::new();
        for index in 0..crate::config::ASSEMBLER_MAX_SEGMENT_COUNT {
            assembler.add(&[1; 8], (index + 1) * 16).unwrap();
        }
        assert_eq!(assembler.add(&[2; 8], 0), Err(AssemblerError));
        assert_eq!(assembler.assembler.peek_front(), 0);
        #[cfg(feature = "proto-ipv4-fragmentation")]
        assert_eq!(assembler.first_ipv4_header(), None);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn packet_assembler_set_releases_oversized_free_buffer() {
        let mut set = PacketAssemblerSet::new();
        let assembler = set.get(&Key { id: 1 }, Instant::ZERO).unwrap();
        assembler
            .set_total_size(REASSEMBLY_BUFFER_SIZE + 1)
            .unwrap();
        assembler
            .add(&vec![0; REASSEMBLY_BUFFER_SIZE + 1], 0)
            .unwrap();
        assert!(assembler.assemble().is_some());

        set.remove_expired(Instant::ZERO);
        let assembler = set.get(&Key { id: 2 }, Instant::ZERO).unwrap();
        assert_eq!(assembler.buffer.capacity(), 0);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn custom_reassembly_storage_failure_and_expiry_preserve_policy() {
        use crate::storage::{OwnedBufferResizeError, PacketBytesStorage};
        fn fail(_: usize) -> Result<PacketBytesStorage, OwnedBufferResizeError> {
            Err(OwnedBufferResizeError)
        }
        #[derive(Debug)]
        struct Bytes(alloc::vec::Vec<u8>);
        impl AsRef<[u8]> for Bytes {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
        impl AsMut<[u8]> for Bytes {
            fn as_mut(&mut self) -> &mut [u8] {
                &mut self.0
            }
        }
        fn mapped(len: usize) -> Result<PacketBytesStorage, OwnedBufferResizeError> {
            Ok(PacketBytesStorage::Owned(alloc::boxed::Box::new(Bytes(
                alloc::vec![0; len],
            ))))
        }
        let mut set = PacketAssemblerSet::new();
        set.set_allocator(fail).unwrap();
        let a = set.get(&Key { id: 9 }, Instant::ZERO).unwrap();
        assert_eq!(a.set_total_size(32), Err(AssemblerError));
        assert_eq!(a.total_size, None);
        assert_eq!(a.buffer.capacity(), 0);
        set.remove_expired(Instant::from_millis(1));
        set.set_allocator(mapped).unwrap();
        let a = set.get(&Key { id: 10 }, Instant::ZERO).unwrap();
        a.set_total_size(32).unwrap();
        a.add(&[0xa5; 32], 0).unwrap();
        assert_eq!(a.assemble(), Some(&[0xa5; 32][..]));
        // The borrowed assembled bytes remain valid until maintenance retires them.
        assert_eq!(a.buffer.capacity(), 32);
        set.remove_expired(Instant::ZERO);
        let a = set.get(&Key { id: 11 }, Instant::ZERO).unwrap();
        assert_eq!(a.buffer.capacity(), 0);
        a.set_total_size(16).unwrap();
        assert_eq!(a.buffer.capacity(), 16);
        a.reset();
        set.remove_expired(Instant::ZERO);
        assert_eq!(
            set.get(&Key { id: 12 }, Instant::ZERO)
                .unwrap()
                .buffer
                .capacity(),
            0
        );
    }

    #[test]
    fn packet_assembler_assemble() {
        let mut p_assembler = PacketAssembler::<Key>::new();

        let data = b"Hello World!";

        p_assembler.set_total_size(data.len()).unwrap();

        p_assembler.add(b"Hello ", 0).unwrap();
        assert_eq!(p_assembler.assemble(), None);

        p_assembler.add(b"World!", b"Hello ".len()).unwrap();

        assert_eq!(p_assembler.assemble(), Some(&b"Hello World!"[..]));
    }

    #[test]
    fn packet_assembler_out_of_order_assemble() {
        let mut p_assembler = PacketAssembler::<Key>::new();

        let data = b"Hello World!";

        p_assembler.set_total_size(data.len()).unwrap();

        p_assembler.add(b"World!", b"Hello ".len()).unwrap();
        assert_eq!(p_assembler.assemble(), None);

        p_assembler.add(b"Hello ", 0).unwrap();

        assert_eq!(p_assembler.assemble(), Some(&b"Hello World!"[..]));
    }

    #[test]
    fn packet_assembler_set() {
        let key = Key { id: 1 };

        let mut set = PacketAssemblerSet::new();

        assert!(set.get(&key, Instant::ZERO).is_ok());
    }

    #[test]
    fn packet_assembler_set_full() {
        let mut set = PacketAssemblerSet::new();
        for i in 0..REASSEMBLY_BUFFER_COUNT {
            set.get(&Key { id: i }, Instant::ZERO).unwrap();
        }
        assert!(set.get(&Key { id: 4 }, Instant::ZERO).is_err());
    }

    #[test]
    fn packet_assembler_set_assembling_many() {
        let mut set = PacketAssemblerSet::new();

        let key = Key { id: 0 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assert_eq!(assr.assemble(), None);
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        // Test that `.assemble()` effectively deletes it.
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assert_eq!(assr.assemble(), None);
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        let key = Key { id: 1 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        let key = Key { id: 2 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.set_total_size(0).unwrap();
        assr.assemble().unwrap();

        let key = Key { id: 2 };
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.set_total_size(2).unwrap();
        assr.add(&[0x00], 0).unwrap();
        assert_eq!(assr.assemble(), None);
        let assr = set.get(&key, Instant::ZERO).unwrap();
        assr.add(&[0x01], 1).unwrap();
        assert_eq!(assr.assemble(), Some(&[0x00, 0x01][..]));
    }
}
