//! Bounded, allocation-free normalization of received TCP GSO frames.
//! The caller retains the complete CPU-owned frame until the cursor finishes.
#![no_std]

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpVersion {
    V4,
    V6,
}

#[derive(Clone, Copy, Debug)]
pub struct PartialChecksum {
    pub start: u16,
    pub offset: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Parameters {
    pub version: IpVersion,
    pub segment_size: u16,
    pub checksum: Option<PartialChecksum>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidLength,
    UnsupportedProtocol,
    InvalidChecksum,
    InvalidMetadata,
    InvalidControl,
}

/// Validated offsets and the original mutable header fields. No packet/header
/// copies are retained. Moving each payload chunk into the consumed prefix is
/// safe because its destination ends before the next unconsumed source byte.
#[derive(Debug)]
pub struct Cursor {
    version: IpVersion,
    frame_len: usize,
    ip: usize,
    tcp: usize,
    payload: usize,
    payload_len: usize,
    sent: usize,
    chunk_size: usize,
    original_seq: u32,
    original_id: u16,
    original_flags: u8,
    original_seed: u16,
    segment_index: u16,
    done: bool,
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}
fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}
fn sum(bytes: &[u8]) -> u64 {
    let mut words = bytes.chunks_exact(2);
    let result = words
        .by_ref()
        .map(|p| u16::from_be_bytes([p[0], p[1]]) as u64)
        .sum::<u64>();
    result
        + words
            .remainder()
            .first()
            .map_or(0, |byte| (*byte as u64) << 8)
}
fn fold(mut value: u64) -> u16 {
    while value >> 16 != 0 {
        value = (value & 0xffff) + (value >> 16);
    }
    value as u16
}

impl Cursor {
    pub fn new(frame: &[u8], parameters: Parameters, mtu: usize) -> Result<Self, Error> {
        if parameters.segment_size == 0 || parameters.segment_size == u16::MAX {
            return Err(Error::InvalidMetadata);
        }
        if frame.len() < 14 {
            return Err(Error::InvalidLength);
        }
        let mut ether_type = u16_at(frame, 12);
        let mut ip = 14;
        // The receive-capacity contract budgets one optional VLAN tag.
        // Preserve it without extending this normalizer into a VLAN stack.
        if ether_type == 0x8100 || ether_type == 0x88a8 {
            if frame.len() - ip < 4 {
                return Err(Error::InvalidLength);
            }
            ether_type = u16_at(frame, ip + 2);
            ip += 4;
            if ether_type == 0x8100 || ether_type == 0x88a8 {
                return Err(Error::UnsupportedProtocol);
            }
        }
        let tcp = match parameters.version {
            IpVersion::V4 => {
                if ether_type != 0x0800 {
                    return Err(Error::UnsupportedProtocol);
                }
                if frame.len() - ip < 20 || frame[ip] >> 4 != 4 {
                    return Err(Error::InvalidLength);
                }
                let ihl = ((frame[ip] & 15) as usize) * 4;
                if ihl < 20
                    || frame.len() - ip < ihl
                    || u16_at(frame, ip + 2) as usize != frame.len() - ip
                {
                    return Err(Error::InvalidLength);
                }
                if frame[ip + 9] != 6 || u16_at(frame, ip + 6) & 0x3fff != 0 {
                    return Err(Error::UnsupportedProtocol);
                }
                if fold(sum(&frame[ip..ip + ihl])) != 0xffff {
                    return Err(Error::InvalidChecksum);
                }
                ip + ihl
            }
            IpVersion::V6 => {
                if ether_type != 0x86dd {
                    return Err(Error::UnsupportedProtocol);
                }
                if frame.len() - ip < 40 || frame[ip] >> 4 != 6 {
                    return Err(Error::InvalidLength);
                }
                let length = u16_at(frame, ip + 4) as usize;
                if length == 0 || length + 40 != frame.len() - ip {
                    return Err(Error::InvalidLength);
                }
                let mut protocol = frame[ip + 6];
                let mut offset = ip + 40;
                while protocol != 6 {
                    // Only headers which leave the ordinary TCP pseudo-header
                    // unchanged. Routing/fragment/AH/ESP require other semantics.
                    if protocol != 0 && protocol != 60 {
                        return Err(Error::UnsupportedProtocol);
                    }
                    if frame.len() - offset < 8 {
                        return Err(Error::InvalidLength);
                    }
                    let length = (frame[offset + 1] as usize + 1) * 8;
                    if length > frame.len() - offset {
                        return Err(Error::InvalidLength);
                    }
                    protocol = frame[offset];
                    offset += length;
                    if offset - ip > mtu {
                        return Err(Error::InvalidLength);
                    }
                }
                offset
            }
        };
        if frame.len() - tcp < 20 {
            return Err(Error::InvalidLength);
        }
        let tcp_header_len = ((frame[tcp + 12] >> 4) as usize) * 4;
        if tcp_header_len < 20 || tcp_header_len > frame.len() - tcp {
            return Err(Error::InvalidLength);
        }
        let payload = tcp + tcp_header_len;
        let payload_len = frame.len() - payload;
        let room = mtu.checked_sub(payload - ip).ok_or(Error::InvalidLength)?;
        if room == 0 && payload_len != 0 {
            return Err(Error::InvalidLength);
        }
        let chunk_size = room.min(parameters.segment_size as usize);
        let multi = payload_len > chunk_size;
        let flags = frame[tcp + 13];
        // Linux TCP output disables large sends in urgent mode. Never duplicate
        // SYN/RST or an urgent pointer across manufactured ordinary segments.
        if multi && flags & (0x02 | 0x04 | 0x20) != 0 {
            return Err(Error::InvalidControl);
        }
        let original_seed = if let Some(checksum) = parameters.checksum {
            if checksum.start as usize != tcp || checksum.offset != 16 {
                return Err(Error::InvalidMetadata);
            }
            u16_at(frame, tcp + 16)
        } else {
            let pseudo = match parameters.version {
                IpVersion::V4 => sum(&frame[ip + 12..ip + 20]),
                IpVersion::V6 => sum(&frame[ip + 8..ip + 40]),
            } + 6
                + (frame.len() - tcp) as u64;
            if fold(pseudo + sum(&frame[tcp..])) != 0xffff {
                return Err(Error::InvalidChecksum);
            }
            fold(pseudo)
        };
        Ok(Self {
            version: parameters.version,
            frame_len: frame.len(),
            ip,
            tcp,
            payload,
            payload_len,
            sent: 0,
            chunk_size,
            original_seq: u32::from_be_bytes(frame[tcp + 4..tcp + 8].try_into().unwrap()),
            original_id: if parameters.version == IpVersion::V4 {
                u16_at(frame, ip + 4)
            } else {
                0
            },
            original_flags: flags,
            original_seed,
            segment_index: 0,
            done: false,
        })
    }

    pub fn is_finished(&self) -> bool {
        self.done
    }
    pub fn source_len(&self) -> usize {
        self.frame_len
    }

    /// Build exactly one ordinary frame. Its slice is frame[..returned_len].
    /// All fallible protocol validation happened in new(), before any segment
    /// can be delivered. Passing the same backing allocation is the contract.
    pub fn next_segment(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.done {
            return None;
        }
        assert!(frame.len() >= self.frame_len);
        let count = (self.payload_len - self.sent).min(self.chunk_size);
        let last = self.sent + count == self.payload_len;
        frame.copy_within(
            self.payload + self.sent..self.payload + self.sent + count,
            self.payload,
        );
        let end = self.payload + count;
        frame[self.tcp + 4..self.tcp + 8].copy_from_slice(
            &self
                .original_seq
                .wrapping_add(self.sent as u32)
                .to_be_bytes(),
        );
        let mut flags = self.original_flags;
        if !last {
            flags &= !(0x01 | 0x08);
        }
        if self.segment_index != 0 {
            flags &= !0x80;
        }
        frame[self.tcp + 13] = flags;
        match self.version {
            IpVersion::V4 => {
                set_u16(frame, self.ip + 2, (end - self.ip) as u16);
                set_u16(
                    frame,
                    self.ip + 4,
                    self.original_id.wrapping_add(self.segment_index),
                );
                set_u16(frame, self.ip + 10, 0);
                set_u16(frame, self.ip + 10, !fold(sum(&frame[self.ip..self.tcp])));
            }
            IpVersion::V6 => set_u16(frame, self.ip + 4, (end - self.ip - 40) as u16),
        }
        // Linux tcp_gso_segment: delta = htonl(~old_tcp_len + new_tcp_len).
        // Retain the original partial seed, including any corruption. Rebuilding
        // a fresh pseudo-header here would turn a bad seed into accepted data.
        let old_tcp_len = (self.frame_len - self.tcp) as u16;
        let new_tcp_len = (end - self.tcp) as u16;
        let adjusted = fold(self.original_seed as u64 + (!old_tcp_len) as u64 + new_tcp_len as u64);
        set_u16(frame, self.tcp + 16, adjusted);
        set_u16(frame, self.tcp + 16, !fold(sum(&frame[self.tcp..end])));
        self.sent += count;
        self.segment_index = self.segment_index.wrapping_add(1);
        self.done = last;
        Some(end)
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests {
    use super::*;
    use std::{vec, vec::Vec};

    fn packet(
        version: IpVersion,
        payload_len: usize,
        partial: bool,
        ip_options: bool,
        tcp_options: bool,
        flags: u8,
    ) -> (Vec<u8>, Parameters, usize) {
        let ip_len = match version {
            IpVersion::V4 => {
                if ip_options {
                    24
                } else {
                    20
                }
            }
            IpVersion::V6 => {
                if ip_options {
                    48
                } else {
                    40
                }
            }
        };
        let tcp_len = if tcp_options { 32 } else { 20 };
        let tcp = 14 + ip_len;
        let mut frame = vec![0; tcp + tcp_len + payload_len];
        frame[..12].copy_from_slice(&[2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        set_u16(
            &mut frame,
            12,
            if version == IpVersion::V4 {
                0x0800
            } else {
                0x86dd
            },
        );
        match version {
            IpVersion::V4 => {
                frame[14] = 0x40 | (ip_len / 4) as u8;
                set_u16(&mut frame, 16, (ip_len + tcp_len + payload_len) as u16);
                set_u16(&mut frame, 18, 0xfffe);
                frame[22] = 64;
                frame[23] = 6;
                frame[26..34].copy_from_slice(&[10, 0, 0, 1, 10, 0, 0, 2]);
                if ip_options {
                    frame[34..38].copy_from_slice(&[1, 1, 1, 1]);
                }
                let check = !fold(sum(&frame[14..tcp]));
                set_u16(&mut frame, 24, check);
            }
            IpVersion::V6 => {
                frame[14] = 0x60;
                set_u16(&mut frame, 18, (ip_len - 40 + tcp_len + payload_len) as u16);
                frame[20] = if ip_options { 0 } else { 6 };
                frame[21] = 64;
                frame[22..38].fill(1);
                frame[38..54].fill(2);
                if ip_options {
                    frame[54] = 6;
                }
            }
        }
        set_u16(&mut frame, tcp, 1234);
        set_u16(&mut frame, tcp + 2, 80);
        frame[tcp + 4..tcp + 8].copy_from_slice(&0xfffffff0u32.to_be_bytes());
        frame[tcp + 12] = ((tcp_len / 4) as u8) << 4;
        frame[tcp + 13] = flags;
        set_u16(&mut frame, tcp + 14, 32768);
        if tcp_options {
            frame[tcp + 20..tcp + tcp_len].fill(1);
        }
        for (i, byte) in frame[tcp + tcp_len..].iter_mut().enumerate() {
            *byte = (i * 37 + 11) as u8;
        }
        let pseudo = match version {
            IpVersion::V4 => sum(&frame[26..34]),
            IpVersion::V6 => sum(&frame[22..54]),
        } + 6
            + (tcp_len + payload_len) as u64;
        set_u16(&mut frame, tcp + 16, fold(pseudo));
        if !partial {
            let checksum = !fold(sum(&frame[tcp..]));
            set_u16(&mut frame, tcp + 16, checksum);
        }
        (
            frame,
            Parameters {
                version,
                segment_size: 1448,
                checksum: partial.then_some(PartialChecksum {
                    start: tcp as u16,
                    offset: 16,
                }),
            },
            tcp,
        )
    }

    fn valid_tcp(frame: &[u8], version: IpVersion, tcp: usize) -> bool {
        let pseudo = match version {
            IpVersion::V4 => sum(&frame[26..34]),
            IpVersion::V6 => sum(&frame[22..54]),
        } + 6
            + (frame.len() - tcp) as u64;
        fold(pseudo + sum(&frame[tcp..])) == 0xffff
    }

    fn check_segments(
        version: IpVersion,
        length: usize,
        partial: bool,
        ip_options: bool,
        tcp_options: bool,
        budget: usize,
    ) {
        let flags = 0x80 | 0x40 | 0x10 | 0x08 | 0x01;
        let (mut frame, parameters, tcp) =
            packet(version, length, partial, ip_options, tcp_options, flags);
        let payload = tcp + ((frame[tcp + 12] >> 4) as usize) * 4;
        let original_header = frame[..payload].to_vec();
        let original_data = frame[payload..].to_vec();
        let mut cursor = Cursor::new(&frame, parameters, 1500).unwrap();
        let mut collected = Vec::new();
        let mut segment = 0;
        while !cursor.is_finished() {
            for _ in 0..budget {
                let Some(end) = cursor.next_segment(&mut frame) else {
                    break;
                };
                let output = &frame[..end];
                assert!(end - 14 <= 1500);
                assert!(valid_tcp(output, version, tcp));
                assert_eq!(&output[..14], &original_header[..14]);
                assert_eq!(
                    &output[tcp + 20..payload],
                    &original_header[tcp + 20..payload]
                );
                assert_eq!(
                    u32::from_be_bytes(output[tcp + 4..tcp + 8].try_into().unwrap()),
                    0xfffffff0u32.wrapping_add(collected.len() as u32)
                );
                if segment != 0 {
                    assert_eq!(output[tcp + 13] & 0x80, 0);
                }
                if !cursor.is_finished() {
                    assert_eq!(output[tcp + 13] & 0x09, 0);
                } else {
                    assert_eq!(output[tcp + 13] & 0x09, flags & 0x09);
                }
                match version {
                    IpVersion::V4 => {
                        assert_eq!(fold(sum(&output[14..tcp])), 0xffff);
                        assert_eq!(u16_at(output, 16) as usize, end - 14);
                        assert_eq!(u16_at(output, 18), 0xfffeu16.wrapping_add(segment));
                        assert_eq!(&output[34..tcp], &original_header[34..tcp]);
                    }
                    IpVersion::V6 => assert_eq!(u16_at(output, 18) as usize, end - 54),
                }
                collected.extend_from_slice(&output[payload..]);
                segment += 1;
            }
        }
        assert_eq!(collected, original_data);
        assert!(cursor.next_segment(&mut frame).is_none());
    }

    #[test]
    fn v4_v6_partial_complete_options_odd_tails_and_budget() {
        for version in [IpVersion::V4, IpVersion::V6] {
            for partial in [false, true] {
                for options in [false, true] {
                    for length in [0, 1, 1448, 1449, 7240, 16385] {
                        check_segments(version, length, partial, options, options, 1);
                        check_segments(version, length, partial, options, options, 2);
                    }
                }
            }
        }
    }

    #[test]
    fn maximum_non_jumbo_v6_frame_exceeds_64k_and_is_fully_consumed() {
        check_segments(IpVersion::V6, 65535 - 32, true, false, true, 1);
    }

    #[test]
    fn bad_partial_seed_is_not_healed() {
        for version in [IpVersion::V4, IpVersion::V6] {
            let (mut frame, parameters, tcp) = packet(version, 7241, true, false, true, 0x10);
            frame[tcp + 16] ^= 1;
            let mut cursor = Cursor::new(&frame, parameters, 1500).unwrap();
            while let Some(end) = cursor.next_segment(&mut frame) {
                assert!(!valid_tcp(&frame[..end], version, tcp));
            }
        }
    }

    #[test]
    fn corrupt_complete_tcp_and_ipv4_checksum_are_rejected_before_output() {
        let (mut frame, parameters, tcp) = packet(IpVersion::V4, 7240, false, true, true, 0x10);
        frame[tcp + 30] ^= 1;
        assert_eq!(
            Cursor::new(&frame, parameters, 1500).unwrap_err(),
            Error::InvalidChecksum
        );
        let (mut frame, parameters, _) = packet(IpVersion::V4, 7240, true, false, true, 0x10);
        frame[24] ^= 1;
        assert_eq!(
            Cursor::new(&frame, parameters, 1500).unwrap_err(),
            Error::InvalidChecksum
        );
    }

    #[test]
    fn metadata_lengths_fragments_and_unsupported_extensions_are_rejected() {
        let (frame, mut parameters, _) = packet(IpVersion::V4, 100, true, false, false, 0x10);
        for size in [0, u16::MAX] {
            parameters.segment_size = size;
            assert_eq!(
                Cursor::new(&frame, parameters, 1500).unwrap_err(),
                Error::InvalidMetadata
            );
        }
        parameters.segment_size = 1448;
        parameters.checksum.as_mut().unwrap().start += 1;
        assert_eq!(
            Cursor::new(&frame, parameters, 1500).unwrap_err(),
            Error::InvalidMetadata
        );
        let (mut frame, parameters, _) = packet(IpVersion::V4, 100, true, false, false, 0x10);
        frame[20] = 0x20;
        assert_eq!(
            Cursor::new(&frame, parameters, 1500).unwrap_err(),
            Error::UnsupportedProtocol
        );
        let (mut frame, parameters, _) = packet(IpVersion::V6, 100, true, true, false, 0x10);
        for protocol in [43, 44, 50, 51] {
            frame[20] = protocol;
            assert_eq!(
                Cursor::new(&frame, parameters, 1500).unwrap_err(),
                Error::UnsupportedProtocol
            );
        }
        // Restore a legal HBH chain so these cases reach extension/TCP
        // bounds checks instead of exiting early on unsupported AH.
        let (valid, parameters, tcp) = packet(IpVersion::V6, 100, true, true, true, 0x10);
        for end in 0..tcp + 32 {
            let mut truncated = valid[..end].to_vec();
            if end >= 54 {
                // Match the declared IP size: the deep failure must be the
                // incomplete extension/TCP header, not merely total length.
                set_u16(&mut truncated, 18, (end - 54) as u16);
            }
            assert!(Cursor::new(&truncated, parameters, 1500).is_err());
        }
        let mut oversized_extension = valid;
        oversized_extension[55] = 255;
        assert_eq!(
            Cursor::new(&oversized_extension, parameters, 1500).unwrap_err(),
            Error::InvalidLength
        );
        let (mut short_tcp, parameters, tcp) = packet(IpVersion::V6, 0, true, false, false, 0x10);
        short_tcp[tcp + 12] = 0xf0;
        assert_eq!(
            Cursor::new(&short_tcp, parameters, 1500).unwrap_err(),
            Error::InvalidLength
        );
    }

    #[test]
    fn control_flags_are_only_supported_for_a_single_normalized_segment() {
        for flag in [0x02, 0x04, 0x20] {
            let (frame, parameters, _) = packet(IpVersion::V4, 4000, true, false, false, flag);
            assert_eq!(
                Cursor::new(&frame, parameters, 1500).unwrap_err(),
                Error::InvalidControl
            );
            for length in [0, 10] {
                let (mut frame, parameters, tcp) =
                    packet(IpVersion::V4, length, true, false, false, flag);
                let mut cursor = Cursor::new(&frame, parameters, 1500).unwrap();
                let end = cursor.next_segment(&mut frame).unwrap();
                assert_eq!(frame[tcp + 13], flag);
                assert!(valid_tcp(&frame[..end], IpVersion::V4, tcp));
            }
        }
    }

    #[test]
    fn large_mss_is_limited_by_mtu_and_skipping_a_token_does_not_repeat_data() {
        let (mut frame, mut parameters, _) = packet(IpVersion::V4, 4000, true, false, false, 0x10);
        parameters.segment_size = 9000;
        let mut cursor = Cursor::new(&frame, parameters, 1500).unwrap();
        assert_eq!(cursor.next_segment(&mut frame), Some(1514));
        // Discard that token. The next segment must start at the next sequence.
        let end = cursor.next_segment(&mut frame).unwrap();
        assert_eq!(end, 1514);
        assert_eq!(
            u32::from_be_bytes(frame[38..42].try_into().unwrap()),
            0xfffffff0u32.wrapping_add(1460)
        );
    }

    #[test]
    fn one_byte_mss_never_overwrites_future_source_payload() {
        let (mut frame, mut parameters, tcp) = packet(IpVersion::V4, 100, true, false, false, 0x10);
        parameters.segment_size = 1;
        let expected = frame[tcp + 20..].to_vec();
        let mut cursor = Cursor::new(&frame, parameters, 1500).unwrap();
        let mut actual = Vec::new();
        while let Some(end) = cursor.next_segment(&mut frame) {
            assert!(valid_tcp(&frame[..end], IpVersion::V4, tcp));
            actual.push(frame[end - 1]);
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn one_vlan_tag_is_preserved_and_nested_tags_are_out_of_scope() {
        for tag in [0x8100u16, 0x88a8] {
            let (original, mut parameters, old_tcp) =
                packet(IpVersion::V4, 4000, true, false, false, 0x10);
            let mut frame = original[..12].to_vec();
            frame.extend_from_slice(&tag.to_be_bytes());
            frame.extend_from_slice(&[0, 7]);
            frame.extend_from_slice(&original[12..]);
            parameters.checksum.as_mut().unwrap().start += 4;
            let tcp = old_tcp + 4;
            let mut cursor = Cursor::new(&frame, parameters, 1500).unwrap();
            while let Some(end) = cursor.next_segment(&mut frame) {
                assert!(end <= 1518);
                assert_eq!(u16_at(&frame, 12), tag);
                assert_eq!(u16_at(&frame, 16), 0x0800);
                let pseudo = sum(&frame[30..38]) + 6 + (end - tcp) as u64;
                assert_eq!(fold(pseudo + sum(&frame[tcp..end])), 0xffff);
            }
            let mut nested = frame[..12].to_vec();
            nested.extend_from_slice(&[0x81, 0, 0, 3]);
            nested.extend_from_slice(&frame[12..]);
            assert_eq!(
                Cursor::new(&nested, parameters, 1500).unwrap_err(),
                Error::UnsupportedProtocol
            );
        }
    }

    #[test]
    fn ipv6_destination_options_are_preserved() {
        let (mut frame, parameters, tcp) = packet(IpVersion::V6, 4001, true, true, true, 0x10);
        frame[20] = 60;
        let original_extension = frame[54..tcp].to_vec();
        let mut cursor = Cursor::new(&frame, parameters, 1500).unwrap();
        while let Some(end) = cursor.next_segment(&mut frame) {
            assert_eq!(&frame[54..tcp], &original_extension);
            assert!(valid_tcp(&frame[..end], IpVersion::V6, tcp));
        }
    }

    #[test]
    fn seed_adjustment_matches_linux_x86_expression() {
        // Independent transcription of tcp_gso_segment's htonl/csum_add/fold,
        // with x86's native __sum16 register representation.
        let mut state = 66u64;
        for _ in 0..200000 {
            let mut next = || {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 32) as u32
            };
            let seed = next() as u16;
            let old = 20 + next() % (65536 - 20);
            let new = 20 + next() % (old - 19);
            let delta = (!old).wrapping_add(new).swap_bytes();
            let native_seed = seed.swap_bytes() as u32;
            let (mut added, carry) = native_seed.overflowing_add(delta);
            added = added.wrapping_add(carry as u32);
            let reference = fold(added as u64).swap_bytes();
            assert_eq!(
                fold(seed as u64 + (!(old as u16)) as u64 + new as u64),
                reference
            );
        }
    }
}
