//! Build one complete IPv4 UDP datagram before the synchronous output hooks.
//!
//! This writes into the output owner's pre-reserved buffer. The caller owns
//! routing, PMTU/DF policy and socket accounting; this module only serializes
//! the IPv4 and UDP headers and computes their checksums.

use smoltcp::{
    phy::{Checksum, ChecksumCapabilities},
    wire::{IpAddress, IpProtocol, Ipv4Address, Ipv4Packet, UdpPacket, UdpRepr},
};
use system_error::SystemError;

const IPV4_HEADER_LEN: usize = 20;
const UDP_HEADER_LEN: usize = 8;

pub(super) fn packet_len(payload_len: usize) -> Result<usize, SystemError> {
    payload_len
        .checked_add(IPV4_HEADER_LEN + UDP_HEADER_LEN)
        .filter(|len| *len <= u16::MAX as usize)
        .ok_or(SystemError::EMSGSIZE)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit(
    output: &mut [u8],
    payload: &[u8],
    source: Ipv4Address,
    destination: Ipv4Address,
    source_port: u16,
    destination_port: u16,
    ttl: u8,
    tos: u8,
    identification: u16,
    dont_fragment: bool,
    checksum_enabled: bool,
) -> Result<usize, SystemError> {
    let total_len = packet_len(payload.len())?;
    let bytes = output.get_mut(..total_len).ok_or(SystemError::EINVAL)?;
    let mut ip = Ipv4Packet::new_unchecked(bytes);
    ip.set_version(4);
    ip.set_header_len(IPV4_HEADER_LEN as u8);
    ip.set_total_len(total_len as u16);
    ip.set_ident(identification);
    ip.clear_flags();
    ip.set_dont_frag(dont_fragment);
    ip.set_more_frags(false);
    ip.set_frag_offset(0);
    ip.set_hop_limit(ttl);
    ip.set_next_header(IpProtocol::Udp);
    ip.set_src_addr(source);
    ip.set_dst_addr(destination);
    ip.set_dscp(tos >> 2);
    ip.set_ecn(tos & 3);

    let udp_repr = UdpRepr {
        src_port: source_port,
        dst_port: destination_port,
    };
    let mut udp = UdpPacket::new_unchecked(ip.payload_mut());
    let mut checksum_caps = ChecksumCapabilities::default();
    if !checksum_enabled {
        checksum_caps.udp = Checksum::None;
    }
    udp_repr.emit(
        &mut udp,
        &IpAddress::Ipv4(source),
        &IpAddress::Ipv4(destination),
        payload.len(),
        |target| target.copy_from_slice(payload),
        &checksum_caps,
    );
    ip.fill_checksum();
    Ok(total_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_udp_with_header_fields_and_checksums() {
        let source = Ipv4Address::new(192, 0, 2, 1);
        let destination = Ipv4Address::new(198, 51, 100, 2);
        let mut output = [0u8; 64];
        let used = emit(
            &mut output,
            b"hello",
            source,
            destination,
            1234,
            4321,
            17,
            0x2e,
            0x1234,
            false,
            true,
        )
        .unwrap();
        assert_eq!(used, 33);
        let ip = Ipv4Packet::new_checked(&output[..used]).unwrap();
        assert_eq!(ip.ident(), 0x1234);
        assert!(!ip.dont_frag());
        assert_eq!(ip.hop_limit(), 17);
        assert_eq!((ip.dscp() << 2) | ip.ecn(), 0x2e);
        assert!(ip.verify_checksum());
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert_eq!(udp.src_port(), 1234);
        assert_eq!(udp.dst_port(), 4321);
        assert_eq!(udp.payload(), b"hello");
        assert!(udp.verify_checksum(&IpAddress::Ipv4(source), &IpAddress::Ipv4(destination)));
    }

    #[test]
    fn rejects_oversized_or_short_reservation_without_writing() {
        assert_eq!(packet_len(65_508), Err(SystemError::EMSGSIZE));
        let mut output = [0xa5u8; 27];
        let result = emit(
            &mut output,
            b"x",
            Ipv4Address::new(127, 0, 0, 1),
            Ipv4Address::new(127, 0, 0, 1),
            1,
            2,
            64,
            0,
            1,
            true,
            true,
        );
        assert_eq!(result, Err(SystemError::EINVAL));
        assert!(output.iter().all(|byte| *byte == 0xa5));
    }

    #[test]
    fn disabled_udp_checksum_stays_zero_without_disabling_ipv4_checksum() {
        let source = Ipv4Address::new(192, 0, 2, 1);
        let destination = Ipv4Address::new(198, 51, 100, 2);
        let mut output = [0u8; 32];
        let used = emit(
            &mut output,
            b"test",
            source,
            destination,
            1234,
            4321,
            64,
            0,
            1,
            true,
            false,
        )
        .unwrap();
        let ip = Ipv4Packet::new_checked(&output[..used]).unwrap();
        assert!(ip.verify_checksum());
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert_eq!(udp.checksum(), 0);
    }
}
