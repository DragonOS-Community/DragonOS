//! Encode actual tuples, including NAT reply keys, for Linux/Docker clients.
use super::super::{nft::append_attr, NetfilterMessage, HEADER_LEN};
use crate::net::conntrack::{CtAddress, CtL4, CtRecord, CtTuple};
use alloc::vec::Vec;
use system_error::SystemError;
fn tuple(bytes: &mut Vec<u8>, kind: u16, tuple: CtTuple) -> Result<(), SystemError> {
    let mut ip = Vec::new();
    match (tuple.src, tuple.dst) {
        (CtAddress::V4(src), CtAddress::V4(dst)) => {
            append_attr(&mut ip, 1, &src)?;
            append_attr(&mut ip, 2, &dst)?;
        }
        (CtAddress::V6(src), CtAddress::V6(dst)) => {
            append_attr(&mut ip, 3, &src)?;
            append_attr(&mut ip, 4, &dst)?;
        }
        _ => return Err(SystemError::EINVAL),
    }
    let mut proto = Vec::new();
    let number = match tuple.l4 {
        CtL4::Tcp { .. } => 6,
        CtL4::Udp { .. } => 17,
        CtL4::Icmp { .. } => 1,
        CtL4::Icmpv6 { .. } => 58,
        CtL4::Generic { protocol } => protocol,
    };
    append_attr(&mut proto, 1, &[number])?;
    match tuple.l4 {
        CtL4::Tcp { src_port, dst_port } | CtL4::Udp { src_port, dst_port } => {
            append_attr(&mut proto, 2, &src_port.to_be_bytes())?;
            append_attr(&mut proto, 3, &dst_port.to_be_bytes())?;
        }
        CtL4::Icmp {
            identifier,
            kind,
            code,
        }
        | CtL4::Icmpv6 {
            identifier,
            kind,
            code,
        } => {
            let base = if number == 1 { 4 } else { 7 };
            append_attr(&mut proto, base, &identifier.to_be_bytes())?;
            append_attr(&mut proto, base + 1, &[kind])?;
            append_attr(&mut proto, base + 2, &[code])?;
        }
        _ => {}
    }
    let mut nested = Vec::new();
    append_attr(&mut nested, 1 | 0x8000, &ip)?;
    append_attr(&mut nested, 2 | 0x8000, &proto)?;
    append_attr(bytes, kind | 0x8000, &nested)
}
pub(super) fn record(
    record: &CtRecord,
    sequence: u32,
    port: u32,
    flags: u16,
) -> Result<NetfilterMessage, SystemError> {
    let mut bytes = Vec::new();
    bytes.try_reserve(256).map_err(|_| SystemError::ENOMEM)?;
    bytes.resize(HEADER_LEN, 0);
    bytes.extend_from_slice(&[
        if matches!(record.original.src, CtAddress::V4(_)) {
            2
        } else {
            10
        },
        0,
        0,
        0,
    ]);
    tuple(&mut bytes, 1, record.original)?;
    tuple(&mut bytes, 2, record.reply)?;
    append_attr(&mut bytes, 3, &record.status.to_be_bytes())?;
    if let Some(state) = record.tcp_state {
        let mut tcp = Vec::new();
        append_attr(&mut tcp, 1, &[state])?;
        let mut info = Vec::new();
        append_attr(&mut info, 1 | 0x8000, &tcp)?;
        append_attr(&mut bytes, 4 | 0x8000, &info)?;
    }
    append_attr(&mut bytes, 7, &record.timeout.to_be_bytes())?;
    append_attr(&mut bytes, 12, &(record.serial as u32).to_be_bytes())?;
    let len = bytes.len() as u32;
    bytes[..4].copy_from_slice(&len.to_ne_bytes());
    bytes[4..6].copy_from_slice(&0x100u16.to_ne_bytes());
    bytes[6..8].copy_from_slice(&flags.to_ne_bytes());
    bytes[8..12].copy_from_slice(&sequence.to_ne_bytes());
    bytes[12..16].copy_from_slice(&port.to_ne_bytes());
    NetfilterMessage::new(bytes)
}
