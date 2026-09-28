use super::*;

#[derive(Debug)]
pub(crate) struct NftRule {
    pub(crate) handle: u64,
    pub(super) expressions: Vec<NftExpression>,
}

impl NftRule {
    pub(super) fn requires_conntrack(&self) -> bool {
        self.expressions.iter().any(|expression| {
            matches!(
                expression,
                NftExpression::CtState { .. }
                    | NftExpression::XtConntrack(_)
                    | NftExpression::Nat { .. }
                    | NftExpression::Masquerade { .. }
                    | NftExpression::Redirect { .. }
                    | NftExpression::XtNatTarget { .. }
            )
        })
    }

    pub(crate) fn verdict(&self) -> NftRuleVerdict {
        self.expressions
            .iter()
            .find_map(|expression| match expression {
                NftExpression::Immediate(verdict) => Some(*verdict),
                _ => None,
            })
            .unwrap_or(NftRuleVerdict::Continue)
    }

    pub(super) fn evaluate(&self, context: &NftPacket<'_>, sets: &[Arc<NftSet>]) -> RuleResult {
        let packet = context.bytes;
        // nft data registers: four legacy 16-byte registers or sixteen
        // 32-bit registers, both addressing the same 64 bytes.
        let mut registers = [0u8; 64];
        let mut transport_offset = None;
        for expression in &self.expressions {
            match expression {
                NftExpression::Payload {
                    base,
                    offset,
                    len,
                    dreg,
                } => {
                    let start = match base {
                        NftPayloadBase::Network => *offset,
                        NftPayloadBase::Transport => {
                            let Some(header_len) = *transport_offset
                                .get_or_insert_with(|| ip_transport_offset(packet))
                            else {
                                return RuleResult::Break;
                            };
                            header_len + offset
                        }
                    };
                    let Some(source) = packet.get(start..start + len) else {
                        return RuleResult::Break;
                    };
                    registers[*dreg..dreg + len].copy_from_slice(source);
                    let aligned_end = (len + 3) & !3;
                    registers[dreg + len..dreg + aligned_end].fill(0);
                }
                NftExpression::Cmp { sreg, op, data } => {
                    let comparison = registers[*sreg..sreg + data.len()].cmp(data);
                    let matches = match op {
                        NftCmpOp::Eq => comparison.is_eq(),
                        NftCmpOp::Neq => !comparison.is_eq(),
                        NftCmpOp::Lt => comparison.is_lt(),
                        NftCmpOp::Lte => !comparison.is_gt(),
                        NftCmpOp::Gt => comparison.is_gt(),
                        NftCmpOp::Gte => !comparison.is_lt(),
                    };
                    if !matches {
                        return RuleResult::Break;
                    }
                }
                NftExpression::Byteorder {
                    sreg,
                    dreg,
                    len,
                    size,
                    ..
                } => {
                    for index in 0..len / size {
                        // Linux 6.6 indexes the 64-bit source through u32
                        // slots, while its destination uses u64 slots.
                        let source = sreg + index * if *size == 8 { 4 } else { *size };
                        let destination = dreg + index * size;
                        let mut value = [0u8; 8];
                        value[..*size].copy_from_slice(&registers[source..source + size]);
                        if cfg!(target_endian = "little") {
                            value[..*size].reverse();
                        }
                        registers[destination..destination + size].copy_from_slice(&value[..*size]);
                    }
                }
                NftExpression::Range { sreg, op, from, to } => {
                    let value = &registers[*sreg..sreg + from.len()];
                    let inside = value >= from.as_slice() && value <= to.as_slice();
                    if inside != matches!(op, NftRangeOp::Eq) {
                        return RuleResult::Break;
                    }
                }
                NftExpression::Bitwise {
                    sreg,
                    dreg,
                    len,
                    operation,
                } => {
                    let words = len.div_ceil(4);
                    // Linux evaluates whole u32 slots directly against the
                    // register file. Its traversal order is observable when
                    // source and destination ranges partially overlap.
                    match operation {
                        NftBitwiseOperation::Bool { mask, xor } => {
                            for index in 0..words {
                                let start = index * 4;
                                let src_offset = sreg + start;
                                let input = u32::from_ne_bytes(
                                    registers[src_offset..src_offset + 4].try_into().unwrap(),
                                );
                                let mut mask_word = [0u8; 4];
                                let mut xor_word = [0u8; 4];
                                let available = len.saturating_sub(start).min(4);
                                mask_word[..available]
                                    .copy_from_slice(&mask[start..start + available]);
                                xor_word[..available]
                                    .copy_from_slice(&xor[start..start + available]);
                                let output = (input & u32::from_ne_bytes(mask_word))
                                    ^ u32::from_ne_bytes(xor_word);
                                let offset = dreg + index * 4;
                                registers[offset..offset + 4]
                                    .copy_from_slice(&output.to_ne_bytes());
                            }
                        }
                        NftBitwiseOperation::Lshift(shift) => {
                            let mut carry = 0;
                            for index in (0..words).rev() {
                                let src_offset = sreg + index * 4;
                                let input = u32::from_ne_bytes(
                                    registers[src_offset..src_offset + 4].try_into().unwrap(),
                                );
                                let output = (input << shift) | carry;
                                carry = if *shift == 0 {
                                    0
                                } else {
                                    input >> (32 - shift)
                                };
                                let offset = dreg + index * 4;
                                registers[offset..offset + 4]
                                    .copy_from_slice(&output.to_ne_bytes());
                            }
                        }
                        NftBitwiseOperation::Rshift(shift) => {
                            let mut carry = 0;
                            for index in 0..words {
                                let src_offset = sreg + index * 4;
                                let input = u32::from_ne_bytes(
                                    registers[src_offset..src_offset + 4].try_into().unwrap(),
                                );
                                let output = carry | (input >> shift);
                                carry = if *shift == 0 {
                                    0
                                } else {
                                    input << (32 - shift)
                                };
                                let offset = dreg + index * 4;
                                registers[offset..offset + 4]
                                    .copy_from_slice(&output.to_ne_bytes());
                            }
                        }
                    }
                }
                NftExpression::Counter(counter) => {
                    counter.count(packet.len() as u64);
                }
                NftExpression::CtState { dreg } => {
                    registers[*dreg..*dreg + 4]
                        .copy_from_slice(&context.ct_state_bits().to_ne_bytes());
                }
                NftExpression::Meta { key, dreg } => match key {
                    NftMetaKey::Len => registers[*dreg..*dreg + 4]
                        .copy_from_slice(&(packet.len() as u32).to_ne_bytes()),
                    NftMetaKey::Protocol => {
                        let protocol: u16 = match packet.first().map(|byte| byte >> 4) {
                            Some(4) => 0x0800,
                            Some(6) => 0x86dd,
                            _ => return RuleResult::Break,
                        };
                        registers[*dreg..*dreg + 4].fill(0);
                        registers[*dreg..*dreg + 2].copy_from_slice(&protocol.to_be_bytes());
                    }
                    NftMetaKey::Mark => {
                        let Some(mark) = context.mark else {
                            return RuleResult::Verdict(NftRuleVerdict::Drop);
                        };
                        registers[*dreg..*dreg + 4].copy_from_slice(&mark.get().to_ne_bytes());
                    }
                    NftMetaKey::Nfproto => {
                        let family = match packet.first().map(|byte| byte >> 4) {
                            Some(4) => 2u8,
                            Some(6) => 10u8,
                            _ => return RuleResult::Break,
                        };
                        registers[*dreg..*dreg + 4].fill(0);
                        registers[*dreg] = family;
                    }
                    NftMetaKey::Iifname => {
                        registers[*dreg..*dreg + 16].copy_from_slice(&context.iifname)
                    }
                    NftMetaKey::Oifname => {
                        registers[*dreg..*dreg + 16].copy_from_slice(&context.oifname)
                    }
                    NftMetaKey::L4proto => {
                        let Some(proto) = ip_protocol(packet) else {
                            return RuleResult::Break;
                        };
                        registers[*dreg..*dreg + 4].fill(0);
                        registers[*dreg] = proto;
                    }
                },
                NftExpression::MetaSetMark { sreg } => {
                    let Some(mark) = context.mark else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    mark.set(u32::from_ne_bytes(
                        registers[*sreg..*sreg + 4].try_into().unwrap(),
                    ));
                }
                NftExpression::FibDaddrType { dreg } => {
                    let Some(addr_type) = context.ipv4_addr_type else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Ok(ipv4) = smoltcp::wire::Ipv4Packet::new_checked(packet) else {
                        return RuleResult::Break;
                    };
                    registers[*dreg..*dreg + 4]
                        .copy_from_slice(&(addr_type(ipv4.dst_addr()) as u32).to_ne_bytes());
                }
                NftExpression::XtTcp(tcp) => match tcp.evaluate(packet) {
                    RuleResult::Continue => {}
                    other => return other,
                },
                NftExpression::XtAddrtype(matcher) => match matcher.evaluate(context) {
                    Some(true) => {}
                    Some(false) => return RuleResult::Break,
                    None => return RuleResult::Verdict(NftRuleVerdict::Drop),
                },
                NftExpression::XtConntrack(matcher) => {
                    if !matcher.evaluate(context) {
                        return RuleResult::Break;
                    }
                }
                NftExpression::Nat {
                    side,
                    family,
                    addr_min,
                    addr_max,
                    port_min,
                    port_max,
                } => {
                    if packet.first().map(|byte| byte >> 4)
                        != Some(if *family == 2 { 4 } else { 6 })
                    {
                        // nft_nat_inet_eval leaves the other protocol alone.
                        continue;
                    }
                    let address = if let Some(min) = addr_min {
                        let len = if *family == 2 { 4 } else { 16 };
                        if addr_max.is_some_and(|max| {
                            registers[*min..*min + len] != registers[max..max + len]
                        }) {
                            return RuleResult::Verdict(NftRuleVerdict::Drop);
                        }
                        Some(if *family == 2 {
                            CtAddress::V4(registers[*min..*min + 4].try_into().unwrap())
                        } else {
                            CtAddress::V6(registers[*min..*min + 16].try_into().unwrap())
                        })
                    } else {
                        None
                    };
                    let Some(ports) = nat_ports(&registers, *port_min, *port_max) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Ok(request) = CtNatRequest::new(address, ports) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    return RuleResult::Nat(match side {
                        NatManipSide::Destination => NftNatAction::Dnat(request),
                        NatManipSide::Source => NftNatAction::Snat(request),
                    });
                }
                NftExpression::Masquerade { port_min, port_max } => {
                    let Some(ports) = nat_ports(&registers, *port_min, *port_max) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    return RuleResult::Nat(NftNatAction::Masquerade { ports });
                }
                NftExpression::Redirect {
                    port_min, port_max, ..
                } => {
                    let Some(resolve) = context.redirect_address else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Some(address) = resolve(packet) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    if !matches!(
                        (packet.first().map(|byte| byte >> 4), address),
                        (Some(4), CtAddress::V4(_)) | (Some(6), CtAddress::V6(_))
                    ) {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    }
                    let Some(ports) = nat_ports(&registers, *port_min, *port_max) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Ok(request) = CtNatRequest::new(Some(address), ports) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    return RuleResult::Nat(NftNatAction::Dnat(request));
                }
                NftExpression::XtNatTarget { action, .. } => return RuleResult::Nat(*action),
                NftExpression::ImmediateData { dreg, data } => {
                    registers[*dreg..*dreg + data.len()].copy_from_slice(data);
                    let aligned_end = (data.len() + 3) & !3;
                    registers[dreg + data.len()..dreg + aligned_end].fill(0);
                }
                NftExpression::Lookup {
                    set_handle,
                    sreg,
                    key_len,
                    dreg,
                    verdict_map,
                    invert,
                } => {
                    // Table sets are appended with monotonically increasing
                    // handles; deletion preserves order and COW replaces in
                    // place. Keep packet lookup logarithmic in set count.
                    let Ok(index) = sets.binary_search_by_key(set_handle, |set| set.handle) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let set = &sets[index];
                    let found = if set.flags & 4 != 0 {
                        let key = &registers[*sreg..*sreg + *key_len];
                        let floor = set
                            .elements
                            .partition_point(|element| element.key.as_slice() <= key)
                            .checked_sub(1);
                        floor.filter(|index| set.elements[*index].flags & 1 == 0)
                    } else {
                        set.element_index(&registers[*sreg..*sreg + *key_len], 0)
                    };
                    match (found, dreg) {
                        (Some(index), Some(0)) if *verdict_map => {
                            let Some(verdict) = set.elements[index].verdict else {
                                return RuleResult::Verdict(NftRuleVerdict::Drop);
                            };
                            return RuleResult::Verdict(verdict);
                        }
                        (Some(index), Some(destination)) => {
                            let Some(value) = set.elements[index].value.as_ref() else {
                                return RuleResult::Verdict(NftRuleVerdict::Drop);
                            };
                            registers[*destination..*destination + value.len()]
                                .copy_from_slice(value);
                            let aligned_end = (value.len() + 3) & !3;
                            registers[destination + value.len()..destination + aligned_end].fill(0);
                        }
                        (found, None) if found.is_some() != *invert => {}
                        _ => return RuleResult::Break,
                    }
                }
                NftExpression::Immediate(verdict) => return RuleResult::Verdict(*verdict),
            }
        }
        RuleResult::Continue
    }

    pub(crate) fn expressions(&self) -> &[NftExpression] {
        &self.expressions
    }
}

/// `None` means an invalid register range; `Some(None)` means no port mapping.
fn nat_ports(
    registers: &[u8; 64],
    min: Option<usize>,
    max: Option<usize>,
) -> Option<Option<CtNatPortRange>> {
    let Some(min) = min else { return Some(None) };
    let first = u16::from_be_bytes(registers[min..min + 2].try_into().unwrap());
    let last = max.map_or(first, |index| {
        u16::from_be_bytes(registers[index..index + 2].try_into().unwrap())
    });
    CtNatPortRange::new(first, last).ok().map(Some)
}

/// Accept only the xt target layouts and range semantics represented by
/// CtNatRequest. In particular random, persistent and address ranges are
/// never silently converted to a single-address mapping.
pub(super) fn parse_xt_nat_target(
    family: u8,
    name: &[u8],
    revision: u32,
    info: &[u8],
) -> Result<NftNatAction, SystemError> {
    if family == 2 && (name == b"DNAT" || name == b"SNAT") && revision == 0 {
        if !(20..=24).contains(&info.len()) || info[20..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        if u32::from_ne_bytes(info[..4].try_into().unwrap()) != 1 {
            return Err(SystemError::EINVAL);
        }
        let flags = u32::from_ne_bytes(info[4..8].try_into().unwrap());
        if flags & !3 != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let address = if flags & 1 != 0 {
            if info[8..12] != info[12..16] {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            Some(CtAddress::V4(info[8..12].try_into().unwrap()))
        } else {
            if info[8..16].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        let ports = if flags & 2 != 0 {
            Some(
                CtNatPortRange::new(
                    u16::from_be_bytes(info[16..18].try_into().unwrap()),
                    u16::from_be_bytes(info[18..20].try_into().unwrap()),
                )
                .map_err(|_| SystemError::EINVAL)?,
            )
        } else {
            if info[16..20].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        let request = CtNatRequest::new(address, ports).map_err(|_| SystemError::EINVAL)?;
        return Ok(if name == b"DNAT" {
            NftNatAction::Dnat(request)
        } else {
            NftNatAction::Snat(request)
        });
    }
    if family == 2 && name == b"MASQUERADE" && revision == 0 {
        if !(20..=24).contains(&info.len()) || info[20..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        if u32::from_ne_bytes(info[..4].try_into().unwrap()) != 1 {
            return Err(SystemError::EINVAL);
        }
        let flags = u32::from_ne_bytes(info[4..8].try_into().unwrap());
        if flags & !2 != 0 || info[8..16].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let ports = if flags & 2 != 0 {
            Some(
                CtNatPortRange::new(
                    u16::from_be_bytes(info[16..18].try_into().unwrap()),
                    u16::from_be_bytes(info[18..20].try_into().unwrap()),
                )
                .map_err(|_| SystemError::EINVAL)?,
            )
        } else {
            if info[16..20].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        return Ok(NftNatAction::Masquerade { ports });
    }
    if family == 10 && name == b"DNAT" && revision == 2 {
        if !(44..=48).contains(&info.len()) || info[44..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        let flags = u32::from_ne_bytes(info[..4].try_into().unwrap());
        if flags & !3 != 0
            || flags & 1 == 0
            || info[4..20] != info[20..36]
            || info[40..44].iter().any(|byte| *byte != 0)
        {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let ports = if flags & 2 != 0 {
            Some(
                CtNatPortRange::new(
                    u16::from_be_bytes(info[36..38].try_into().unwrap()),
                    u16::from_be_bytes(info[38..40].try_into().unwrap()),
                )
                .map_err(|_| SystemError::EINVAL)?,
            )
        } else {
            if info[36..40].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        let address = CtAddress::V6(info[4..20].try_into().unwrap());
        let request = CtNatRequest::new(Some(address), ports).map_err(|_| SystemError::EINVAL)?;
        return Ok(NftNatAction::Dnat(request));
    }
    Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
}

fn ipv4_transport_offset(packet: &[u8]) -> Option<usize> {
    let first = *packet.first()?;
    let header_len = usize::from(first & 0x0f) * 4;
    if first >> 4 != 4 || header_len < 20 || packet.len() < header_len {
        return None;
    }
    let fragment = packet.get(6..8)?;
    if fragment[0] & 0x1f != 0 || fragment[1] != 0 {
        return None;
    }
    Some(header_len)
}

/// Match Linux 6.6 `ipv6_find_hdr(..., target = -1, IP6_FH_F_AUTH)`:
/// AH is the terminal protocol, while non-first fragments expose their
/// next-header value to meta but cannot expose transport payload bytes.
fn ipv6_transport_info(packet: &[u8]) -> Option<(u8, usize, bool)> {
    if packet.len() < 40 || packet[0] >> 4 != 6 {
        return None;
    }
    let payload_len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    if packet.len() < 40 + payload_len || (payload_len == 0 && packet[6] == 0) {
        // A zero-length fixed header is valid unless it indicates a Jumbo
        // Payload hop-by-hop option, which this receive path cannot parse.
        return None;
    }
    let end = 40 + payload_len;
    let packet = &packet[..end];
    let mut next = packet[6];
    let mut offset = 40;
    loop {
        match next {
            0 | 43 | 60 | 44 | 51 => {
                let header = packet.get(offset..offset + 2)?;
                if next == 51 {
                    // Linux's nft packet-info parser stops at AH.
                    return Some((next, offset, false));
                }
                let new_next = header[0];
                let len = if next == 44 {
                    let fragment = packet.get(offset + 2..offset + 4)?;
                    if u16::from_be_bytes([fragment[0], fragment[1]]) & 0xfff8 != 0 {
                        if matches!(new_next, 0 | 43 | 44 | 51 | 60) {
                            return None;
                        }
                        // ipv6_find_hdr returns here without publishing its
                        // local `start` through the output offset pointer.
                        return Some((new_next, 0, true));
                    }
                    8
                } else {
                    (usize::from(header[1]) + 1) * 8
                };
                offset = offset.checked_add(len)?;
                if offset > end {
                    return None;
                }
                next = new_next;
            }
            _ => return Some((next, offset, false)),
        }
    }
}

fn ip_transport_offset(packet: &[u8]) -> Option<usize> {
    match packet.first()? >> 4 {
        4 => ipv4_transport_offset(packet),
        6 => ipv6_transport_info(packet)
            .and_then(|(_, offset, fragment)| (!fragment).then_some(offset)),
        _ => None,
    }
}

#[cfg(test)]
mod ipv6_packet_info_tests {
    use super::{ip_protocol, ip_transport_offset, ipv6_transport_info};

    fn packet(next: u8, payload: &[u8]) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec![0; 40];
        bytes[0] = 0x60;
        bytes[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        bytes[6] = next;
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn extension_chain_locates_transport_without_exposing_padding() {
        let mut payload = [0u8; 24];
        payload[0] = 43; // Hop-by-hop -> routing.
        payload[8] = 60; // Routing -> destination options.
        payload[16] = 17; // Destination options -> UDP.
        let bytes = packet(0, &payload);
        assert_eq!(ip_protocol(&bytes), Some(17));
        assert_eq!(ip_transport_offset(&bytes), Some(64));
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0; 16]);
        assert_eq!(ip_transport_offset(&padded), Some(64));
        padded[4..6].copy_from_slice(&8u16.to_be_bytes());
        assert_eq!(ip_protocol(&padded), None);
    }

    #[test]
    fn nonfirst_fragment_retains_meta_protocol_but_not_transport_payload() {
        let mut fragment = [0u8; 8];
        fragment[0] = 17;
        fragment[2..4].copy_from_slice(&8u16.to_be_bytes());
        let bytes = packet(44, &fragment);
        assert_eq!(ipv6_transport_info(&bytes), Some((17, 0, true)));
        assert_eq!(ip_protocol(&bytes), Some(17));
        assert_eq!(ip_transport_offset(&bytes), None);
        fragment[0] = 60; // An extension after a nonfirst fragment is unknown.
        assert_eq!(ip_protocol(&packet(44, &fragment)), None);
    }

    #[test]
    fn authentication_header_is_terminal_for_linux_nft_packet_info() {
        let bytes = packet(51, &[17, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(ipv6_transport_info(&bytes), Some((51, 40, false)));
        assert_eq!(ip_transport_offset(&bytes), Some(40));
    }

    #[test]
    fn empty_and_truncated_ipv6_payloads_do_not_read_l2_padding() {
        assert_eq!(ip_protocol(&packet(59, &[])), Some(59));
        assert_eq!(ip_transport_offset(&packet(17, &[])), Some(40));
        assert_eq!(ip_protocol(&packet(0, &[])), None);

        let mut truncated = packet(60, &[17, 1, 0, 0, 0, 0, 0, 0]);
        truncated.extend_from_slice(&[0; 8]); // Link-layer padding is not payload.
        assert_eq!(ip_protocol(&truncated), None);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuleResult {
    Break,
    Continue,
    Verdict(NftRuleVerdict),
    Nat(NftNatAction),
}

pub(super) enum ChainResult {
    Verdict(NftVerdict),
    Nat(NftNatAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftNatAction {
    Dnat(CtNatRequest),
    Snat(CtNatRequest),
    Masquerade { ports: Option<CtNatPortRange> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftNatEvent {
    Begin(NatManipSide),
    Rule(NftNatAction),
    Finish(NatManipSide),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftNatProgress {
    Continue,
    SkipRules,
}

/// Names in the netlink request are resolved against the transaction's
/// candidate table. Published rules refer to stable handles, never to an Arc
/// of a chain that a later copy-on-write update may replace.
#[derive(Clone, Copy)]
pub(crate) enum NftRuleInput<'a> {
    Accept,
    Drop,
    Continue,
    Return,
    Jump(&'a [u8]),
    Goto(&'a [u8]),
}

pub(crate) enum NftExpressionInput<'a> {
    Immediate(NftRuleInput<'a>),
    ImmediateData {
        dreg: u32,
        data: &'a [u8],
    },
    Lookup {
        set: Option<&'a [u8]>,
        set_id: Option<u32>,
        sreg: u32,
        dreg: Option<u32>,
        invert: bool,
    },
    XtTcp(&'a [u8]),
    XtAddrtype(&'a [u8]),
    XtConntrack {
        revision: u32,
        info: &'a [u8],
    },
    Ct {
        key: u32,
        dreg: u32,
        direction: Option<u8>,
    },
    Nat {
        nat_type: u32,
        family: u32,
        addr_min_reg: Option<u32>,
        addr_max_reg: Option<u32>,
        proto_min_reg: Option<u32>,
        proto_max_reg: Option<u32>,
        flags: u32,
    },
    Masq {
        flags: u32,
        proto_min_reg: Option<u32>,
        proto_max_reg: Option<u32>,
    },
    Redirect {
        flags: Option<u32>,
        proto_min_reg: Option<u32>,
        proto_max_reg: Option<u32>,
    },
    XtTarget {
        name: &'a [u8],
        revision: u32,
        info: &'a [u8],
    },
    Meta {
        key: u32,
        dreg: u32,
    },
    MetaSet {
        key: u32,
        sreg: u32,
    },
    Fib {
        dreg: u32,
        result: u32,
        flags: u32,
    },
    Payload {
        dreg: u32,
        base: u32,
        offset: u32,
        len: u32,
    },
    Cmp {
        sreg: u32,
        op: u32,
        data: &'a [u8],
    },
    Byteorder {
        sreg: u32,
        dreg: u32,
        op: u32,
        len: u32,
        size: u32,
    },
    Range {
        sreg: u32,
        op: u32,
        from: &'a [u8],
        to: &'a [u8],
    },
    Bitwise {
        sreg: u32,
        dreg: u32,
        len: u32,
        op: u32,
        mask: Option<&'a [u8]>,
        xor: Option<&'a [u8]>,
        data: Option<&'a [u8]>,
    },
    Counter {
        bytes: u64,
        packets: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftRuleVerdict {
    Accept,
    Drop,
    Continue,
    Return,
    Jump(u64),
    Goto(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftVerdict {
    Accept,
    Drop,
}

#[derive(Debug)]
pub(crate) enum NftExpression {
    Immediate(NftRuleVerdict),
    ImmediateData {
        dreg: usize,
        data: Vec<u8>,
    },
    Lookup {
        set_handle: u64,
        sreg: usize,
        key_len: usize,
        dreg: Option<usize>,
        verdict_map: bool,
        invert: bool,
    },
    XtTcp(NftXtTcp),
    XtAddrtype(NftXtAddrtype),
    XtConntrack(NftXtConntrack),
    CtState {
        dreg: usize,
    },
    Nat {
        side: NatManipSide,
        family: u8,
        addr_min: Option<usize>,
        addr_max: Option<usize>,
        port_min: Option<usize>,
        port_max: Option<usize>,
    },
    Masquerade {
        port_min: Option<usize>,
        port_max: Option<usize>,
    },
    Redirect {
        flags: u32,
        port_min: Option<usize>,
        port_max: Option<usize>,
    },
    XtNatTarget {
        name: &'static [u8],
        revision: u32,
        info: Vec<u8>,
        action: NftNatAction,
    },
    Meta {
        key: NftMetaKey,
        dreg: usize,
    },
    MetaSetMark {
        sreg: usize,
    },
    FibDaddrType {
        dreg: usize,
    },
    Payload {
        base: NftPayloadBase,
        offset: usize,
        len: usize,
        dreg: usize,
    },
    Cmp {
        sreg: usize,
        op: NftCmpOp,
        data: Vec<u8>,
    },
    Byteorder {
        sreg: usize,
        dreg: usize,
        len: usize,
        size: usize,
        op: NftByteorderOp,
    },
    Range {
        sreg: usize,
        op: NftRangeOp,
        from: Vec<u8>,
        to: Vec<u8>,
    },
    Bitwise {
        sreg: usize,
        dreg: usize,
        len: usize,
        operation: NftBitwiseOperation,
    },
    Counter(NftCounter),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftMetaKey {
    Len,
    Protocol,
    Mark,
    Nfproto,
    Iifname,
    Oifname,
    L4proto,
}

impl NftMetaKey {
    pub(crate) const fn uapi(self) -> u32 {
        match self {
            Self::Len => 0,
            Self::Protocol => 1,
            Self::Mark => 3,
            Self::Nfproto => 15,
            Self::Iifname => 6,
            Self::Oifname => 7,
            Self::L4proto => 16,
        }
    }
}

fn ipv4_protocol(packet: &[u8]) -> Option<u8> {
    let header = *packet.first()?;
    let header_len = usize::from(header & 0x0f) * 4;
    if header >> 4 != 4 || header_len < 20 || packet.len() < header_len {
        return None;
    }
    packet.get(9).copied()
}

fn ip_protocol(packet: &[u8]) -> Option<u8> {
    match packet.first()? >> 4 {
        4 => ipv4_protocol(packet),
        6 => ipv6_transport_info(packet).map(|(protocol, _, _)| protocol),
        _ => None,
    }
}

/// iptables-nft's `addrtype --dst-type LOCAL` uses xt revision 1 on the
/// wire, even though `nft list ruleset` prints a `fib` shorthand. Until the
/// other addrtype masks and interface restrictions have routing-context tests,
/// reject them instead of claiming a rule with different semantics works.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NftXtAddrtype {
    destination_mask: u16,
}

impl NftXtAddrtype {
    pub(super) fn from_info(info: &[u8]) -> Result<Self, SystemError> {
        if info.len() != 8 {
            return Err(SystemError::EINVAL);
        }
        let source_mask = u16::from_ne_bytes([info[0], info[1]]);
        let destination_mask = u16::from_ne_bytes([info[2], info[3]]);
        let flags = u32::from_ne_bytes(info[4..8].try_into().unwrap());
        if source_mask != 0 || destination_mask != (1 << crate::net::route::RTN_LOCAL) || flags != 0
        {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        Ok(Self { destination_mask })
    }

    pub(crate) fn info(&self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[2..4].copy_from_slice(&self.destination_mask.to_ne_bytes());
        bytes
    }

    /// `None` means the packet path omitted its protected routing view. That
    /// is a hard drop, not a failed match that could bypass a firewall jump.
    fn evaluate(&self, context: &NftPacket<'_>) -> Option<bool> {
        match context.bytes.first().map(|byte| byte >> 4) {
            Some(4) => {
                let Ok(packet) = smoltcp::wire::Ipv4Packet::new_checked(context.bytes) else {
                    return Some(false);
                };
                let addr_type = context.ipv4_addr_type?;
                let kind = addr_type(packet.dst_addr());
                Some(self.destination_mask & (1u16 << kind.min(15)) != 0)
            }
            Some(6) => {
                let Ok(packet) = smoltcp::wire::Ipv6Packet::new_checked(context.bytes) else {
                    return Some(false);
                };
                let is_local = context.ipv6_local_destination?;
                Some(is_local(packet.dst_addr()))
            }
            _ => Some(false),
        }
    }
}

/// The state-only subset of Linux's xt_conntrack revisions 1-3. Reject any
/// tuple, status, expiry or NAT-state predicate until its execution exists.
#[derive(Debug)]
pub(crate) struct NftXtConntrack {
    revision: u32,
    info: Vec<u8>,
    state_mask: u16,
    invert: bool,
}

impl NftXtConntrack {
    pub(super) fn from_info(revision: u32, info: &[u8]) -> Result<Self, SystemError> {
        let size = match revision {
            1 => 152,
            2 => 156,
            3 => 164,
            _ => return Err(SystemError::ENOENT),
        };
        let aligned = (size + 7) & !7;
        if info.len() < size || info.len() > aligned || info[size..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        // xt_conntrack_mtinfo{1,2,3}: four address/mask pairs occupy
        // 0..128, expiry 128..136, protocol 136..138, ports 138..146.
        if info[..146].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let match_flags = u16::from_ne_bytes(info[146..148].try_into().unwrap());
        let invert_flags = u16::from_ne_bytes(info[148..150].try_into().unwrap());
        let state_mask = if revision == 1 {
            if info[151] != 0 {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            u16::from(info[150])
        } else {
            if info[152..156].iter().any(|byte| *byte != 0)
                || (revision == 3 && info[156..164].iter().any(|byte| *byte != 0))
            {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            u16::from_ne_bytes(info[150..152].try_into().unwrap())
        };
        // Linux's UNTRACKED bit is 1 << 9 in xt, versus 1 << 6 in
        // nf_tables. SNAT/DNAT bits require NAT status and remain unsupported.
        if match_flags != 1 || invert_flags & !1 != 0 || state_mask & !0x020f != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        Ok(Self {
            revision,
            info: copy_bytes(info)?,
            state_mask,
            invert: invert_flags != 0,
        })
    }

    pub(crate) fn revision(&self) -> u32 {
        self.revision
    }

    pub(crate) fn info(&self) -> &[u8] {
        &self.info
    }

    fn evaluate(&self, context: &NftPacket<'_>) -> bool {
        let native = context.ct_state_bits();
        let xt_state = (native & 0x0f) | ((native & (1 << 6)) << 3);
        (self.state_mask & xt_state as u16 != 0) ^ self.invert
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NftXtTcp {
    source_ports: [u16; 2],
    dest_ports: [u16; 2],
    option: u8,
    flags_mask: u8,
    flags_match: u8,
    invert: u8,
}

impl NftXtTcp {
    pub(super) fn from_info(info: &[u8]) -> Result<Self, SystemError> {
        // xt_check_match compares XT_ALIGN(user length) with the aligned
        // 12-byte xt_tcp matchsize. Its four port bounds are host-endian.
        if !(12..=16).contains(&info.len()) {
            return Err(SystemError::EINVAL);
        }
        if info[11] & !0x0f != 0 {
            return Err(SystemError::EINVAL);
        }
        let port = |index| u16::from_ne_bytes([info[index], info[index + 1]]);
        Ok(Self {
            source_ports: [port(0), port(2)],
            dest_ports: [port(4), port(6)],
            option: info[8],
            flags_mask: info[9],
            flags_match: info[10],
            invert: info[11],
        })
    }

    pub(crate) fn info(&self) -> [u8; 16] {
        let mut info = [0u8; 16];
        for (index, port) in self
            .source_ports
            .iter()
            .chain(self.dest_ports.iter())
            .enumerate()
        {
            info[index * 2..index * 2 + 2].copy_from_slice(&port.to_ne_bytes());
        }
        info[8..12].copy_from_slice(&[self.option, self.flags_mask, self.flags_match, self.invert]);
        info
    }

    fn evaluate(&self, packet: &[u8]) -> RuleResult {
        let Some(first) = packet.first() else {
            return RuleResult::Break;
        };
        let header_len = usize::from(first & 0x0f) * 4;
        if first >> 4 != 4 || header_len < 20 || packet.len() < header_len {
            return RuleResult::Break;
        }
        let total_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
        if total_len < header_len || total_len > packet.len() {
            return RuleResult::Break;
        }
        let packet = &packet[..total_len];
        let Some(fragment_bytes) = packet.get(6..8) else {
            return RuleResult::Break;
        };
        let fragment = u16::from_be_bytes(fragment_bytes.try_into().unwrap()) & 0x1fff;
        if fragment != 0 {
            return if fragment == 1 {
                RuleResult::Verdict(NftRuleVerdict::Drop)
            } else {
                RuleResult::Break
            };
        }
        let Some(tcp) = packet.get(header_len..header_len + 20) else {
            return RuleResult::Verdict(NftRuleVerdict::Drop);
        };
        for (offset, range, invert) in [
            (0, self.source_ports, self.invert & 0x01 != 0),
            (2, self.dest_ports, self.invert & 0x02 != 0),
        ] {
            let port = u16::from_be_bytes([tcp[offset], tcp[offset + 1]]);
            if !((range[0] <= port && port <= range[1]) ^ invert) {
                return RuleResult::Break;
            }
        }
        if ((tcp[13] & self.flags_mask) == self.flags_match) == (self.invert & 0x04 != 0) {
            return RuleResult::Break;
        }
        if self.option != 0 {
            let tcp_header_len = usize::from(tcp[12] >> 4) * 4;
            if tcp_header_len < 20 {
                return RuleResult::Verdict(NftRuleVerdict::Drop);
            }
            let Some(options) = packet.get(header_len + 20..header_len + tcp_header_len) else {
                return RuleResult::Verdict(NftRuleVerdict::Drop);
            };
            let mut cursor = 0;
            let mut found = false;
            while cursor < options.len() {
                if options[cursor] == self.option {
                    found = true;
                    break;
                }
                if options[cursor] < 2 || cursor + 1 == options.len() {
                    cursor += 1;
                } else {
                    cursor += usize::from(options[cursor + 1].max(1));
                }
            }
            if found == (self.invert & 0x08 != 0) {
                return RuleResult::Break;
            }
        }
        RuleResult::Continue
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum NftPayloadBase {
    Network = 1,
    Transport = 2,
}

#[derive(Debug)]
pub(crate) enum NftBitwiseOperation {
    Bool { mask: Vec<u8>, xor: Vec<u8> },
    Lshift(u32),
    Rshift(u32),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftCmpOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftByteorderOp {
    NetworkToHost,
    HostToNetwork,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftRangeOp {
    Eq,
    Neq,
}

#[derive(Debug)]
pub(crate) struct NftCounter {
    initial_bytes: u64,
    initial_packets: u64,
    shards: Vec<SpinLock<NftCounterPair>>,
}

#[derive(Debug, Default)]
struct NftCounterPair {
    bytes: u64,
    packets: u64,
}

impl NftCounter {
    pub(super) fn new(bytes: u64, packets: u64) -> Result<Self, SystemError> {
        let mut shards = Vec::new();
        shards
            .try_reserve_exact(PerCpu::MAX_CPU_NUM as usize)
            .map_err(|_| SystemError::ENOMEM)?;
        for _ in 0..PerCpu::MAX_CPU_NUM {
            shards.push(SpinLock::new(NftCounterPair::default()));
        }
        Ok(Self {
            initial_bytes: bytes,
            initial_packets: packets,
            shards,
        })
    }

    fn count(&self, length: u64) {
        // Serialize the pair without introducing a global packet-path lock.
        let cpu = smp_get_processor_id().data() as usize;
        let mut pair = self.shards[cpu].lock_irqsave();
        pair.bytes = pair.bytes.wrapping_add(length);
        pair.packets = pair.packets.wrapping_add(1);
    }

    pub(crate) fn snapshot(&self) -> (u64, u64) {
        let mut bytes = self.initial_bytes;
        let mut packets = self.initial_packets;
        for shard in &self.shards {
            let pair = shard.lock_irqsave();
            bytes = bytes.wrapping_add(pair.bytes);
            packets = packets.wrapping_add(pair.packets);
        }
        (bytes, packets)
    }
}

pub(super) fn data_register(register: u32, len: usize) -> Result<usize, SystemError> {
    let first = match register {
        1..=4 => (register as usize - 1) * 16,
        8..=23 => (register as usize - 8) * 4,
        _ => return Err(SystemError::ERANGE),
    };
    if len == 0 {
        return Err(SystemError::EINVAL);
    }
    if first.checked_add(len).is_none_or(|end| end > 64) {
        return Err(SystemError::ERANGE);
    }
    Ok(first)
}
