//! Serialize one complete IPv6 UDP datagram into an admitted output buffer.

use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{IpAddress, IpProtocol, Ipv6Address, Ipv6Packet, UdpPacket, UdpRepr},
};
use system_error::SystemError;

const IPV6_HEADER_LEN: usize = 40;
const UDP_HEADER_LEN: usize = 8;

pub(super) fn packet_len(payload_len: usize) -> Result<usize, SystemError> {
    payload_len
        .checked_add(UDP_HEADER_LEN)
        .filter(|len| *len <= u16::MAX as usize)
        .and_then(|len| len.checked_add(IPV6_HEADER_LEN))
        .ok_or(SystemError::EMSGSIZE)
}

pub(super) fn emit(
    output: &mut [u8],
    payload: &[u8],
    source: Ipv6Address,
    destination: Ipv6Address,
    source_port: u16,
    destination_port: u16,
    hop_limit: u8,
) -> Result<usize, SystemError> {
    let total_len = packet_len(payload.len())?;
    let bytes = output.get_mut(..total_len).ok_or(SystemError::EINVAL)?;
    let mut ip = Ipv6Packet::new_unchecked(bytes);
    ip.set_version(6);
    ip.set_traffic_class(0);
    ip.set_flow_label(0);
    ip.set_payload_len((UDP_HEADER_LEN + payload.len()) as u16);
    ip.set_next_header(IpProtocol::Udp);
    ip.set_hop_limit(hop_limit);
    ip.set_src_addr(source);
    ip.set_dst_addr(destination);

    let udp_repr = UdpRepr {
        src_port: source_port,
        dst_port: destination_port,
    };
    let mut udp = UdpPacket::new_unchecked(ip.payload_mut());
    udp_repr.emit(
        &mut udp,
        &IpAddress::Ipv6(source),
        &IpAddress::Ipv6(destination),
        payload.len(),
        |target| target.copy_from_slice(payload),
        &ChecksumCapabilities::default(),
    );
    Ok(total_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_ipv6_udp_header_and_mandatory_checksum() {
        let source = Ipv6Address::LOCALHOST;
        let destination = Ipv6Address::LOCALHOST;
        let mut output = [0u8; 64];
        let used = emit(&mut output, b"hello", source, destination, 1234, 4321, 64).unwrap();
        assert_eq!(used, 53);
        let ip = Ipv6Packet::new_checked(&output[..used]).unwrap();
        assert_eq!(ip.src_addr(), source);
        assert_eq!(ip.dst_addr(), destination);
        assert_eq!(ip.hop_limit(), 64);
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert_eq!(udp.payload(), b"hello");
        assert_ne!(udp.checksum(), 0);
        assert!(udp.verify_checksum(&source.into(), &destination.into()));
    }

    #[test]
    fn enforces_ipv6_payload_limit_before_touching_buffer() {
        assert_eq!(packet_len(65_527), Ok(65_575));
        assert_eq!(packet_len(65_528), Err(SystemError::EMSGSIZE));
        let mut output = [0xa5; 47];
        assert_eq!(
            emit(
                &mut output,
                b"x",
                Ipv6Address::LOCALHOST,
                Ipv6Address::LOCALHOST,
                1,
                2,
                64,
            ),
            Err(SystemError::EINVAL)
        );
        assert!(output.iter().all(|byte| *byte == 0xa5));
    }
}
