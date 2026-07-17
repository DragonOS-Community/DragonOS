use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::Ordering;
use system_error::SystemError;

use crate::bpf::classic::{
    BpfWidth, ClassicBpfInput, SKF_AD_CPU, SKF_AD_HATYPE, SKF_AD_IFINDEX, SKF_AD_MARK,
    SKF_AD_NLATTR, SKF_AD_NLATTR_NEST, SKF_AD_PAY_OFFSET, SKF_AD_PKTTYPE, SKF_AD_PROTOCOL,
    SKF_AD_QUEUE, SKF_AD_RANDOM, SKF_AD_RXHASH, SKF_AD_VLAN_TAG, SKF_AD_VLAN_TAG_PRESENT,
    SKF_AD_VLAN_TPID, SKF_LL_OFF, SKF_NET_OFF,
};
use crate::filesystem::vfs::iov::IoVecs;
use crate::net::socket::endpoint::{Endpoint, LinkLayerEndpoint};
use crate::net::socket::unix::utils::CmsgBuffer;
use crate::net::socket::PMSG;

use super::uapi::{SOL_PACKET, TP_STATUS_USER, TP_STATUS_VLAN_TPID_VALID, TP_STATUS_VLAN_VALID};
use super::{
    eth_protocol, packet_option, PacketIngressMetadata, PacketMetadata, PacketSocket,
    PacketSocketType, PacketType, ReceivedPacket, SockAddrLl, TpacketAuxdata,
};

const ETHERNET_HEADER_LEN: usize = 14;
const MAX_FLOW_DISSECT_HDRS: u8 = 15;

#[derive(Clone, Copy)]
enum FlowDissectRet {
    StopGood,
    Bad,
    ProtoAgain,
    IpProtoAgain,
    Continue,
}

#[derive(Clone, Copy)]
struct BasicFlowKeys {
    thoff: usize,
    ip_proto: u8,
    is_fragment: bool,
    is_first_fragment: bool,
}

struct ParsedFrame {
    dst: [u8; 6],
    src: [u8; 6],
    protocol: u16,
    vlan: Option<(u16, u16)>,
}

/// Linux receive taps run after the outer inline VLAN header has been moved
/// into skb metadata, while outgoing taps retain an inline VLAN header. This
/// view models both layouts with at most two borrowed segments and is shared
/// by cBPF loads and the eventual queue copy.
struct PacketFilterInput<'a> {
    first: &'a [u8],
    second: &'a [u8],
    data_offset: usize,
    network_offset: usize,
    protocol: u16,
    vlan: Option<(u16, u16)>,
    ingress: PacketIngressMetadata,
    payload_offset_cache: Cell<Option<u32>>,
}

impl<'a> PacketFilterInput<'a> {
    fn new(
        frame: &'a [u8],
        parsed: &ParsedFrame,
        sock_type: PacketSocketType,
        ingress: PacketIngressMetadata,
    ) -> Self {
        let normalize_vlan = parsed.vlan.is_some() && ingress.pkt_type != PacketType::Outgoing;
        let (first, second) = if normalize_vlan {
            (&frame[..12], &frame[16..])
        } else {
            (frame, &[][..])
        };
        let network_offset = if parsed.vlan.is_some() && !normalize_vlan {
            ETHERNET_HEADER_LEN + 4
        } else {
            ETHERNET_HEADER_LEN
        };
        let vlan = if normalize_vlan { parsed.vlan } else { None };
        Self {
            first,
            second,
            data_offset: if sock_type == PacketSocketType::Raw {
                0
            } else {
                network_offset
            },
            network_offset,
            protocol: if normalize_vlan {
                parsed.protocol
            } else {
                parsed.vlan.map_or(parsed.protocol, |(_, tpid)| tpid)
            },
            vlan,
            ingress,
            payload_offset_cache: Cell::new(None),
        }
    }

    #[inline]
    fn full_len(&self) -> usize {
        self.first.len().saturating_add(self.second.len())
    }

    #[inline]
    fn data_len(&self) -> usize {
        self.full_len().saturating_sub(self.data_offset)
    }

    #[inline]
    fn byte_at_full(&self, offset: usize) -> Option<u8> {
        if offset < self.first.len() {
            self.first.get(offset).copied()
        } else {
            self.second
                .get(offset.checked_sub(self.first.len())?)
                .copied()
        }
    }

    fn load_full(&self, offset: usize, width: BpfWidth) -> Option<u32> {
        let width = match width {
            BpfWidth::Word => 4,
            BpfWidth::Half => 2,
            BpfWidth::Byte => 1,
        };
        offset
            .checked_add(width)
            .filter(|end| *end <= self.full_len())?;
        let mut bytes = [0u8; 4];
        for (index, byte) in bytes[..width].iter_mut().enumerate() {
            *byte = self.byte_at_full(offset.checked_add(index)?)?;
        }
        Some(match width {
            4 => u32::from_be_bytes(bytes),
            2 => u16::from_be_bytes([bytes[0], bytes[1]]) as u32,
            1 => bytes[0] as u32,
            _ => unreachable!(),
        })
    }

    fn resolve_offset(&self, offset: i32) -> Option<usize> {
        let absolute = if offset >= 0 {
            (self.data_offset as i64).checked_add(offset as i64)?
        } else if offset >= SKF_NET_OFF {
            (self.network_offset as i64).checked_add((offset - SKF_NET_OFF) as i64)?
        } else if offset >= SKF_LL_OFF {
            (offset - SKF_LL_OFF) as i64
        } else {
            return None;
        };
        usize::try_from(absolute).ok()
    }

    fn copy_prefix_to(&self, output: &mut Vec<u8>, len: usize) {
        let end = self.data_offset + len;
        let first_start = self.data_offset.min(self.first.len());
        let first_end = end.min(self.first.len());
        if first_start < first_end {
            output.extend_from_slice(&self.first[first_start..first_end]);
        }
        if end > self.first.len() {
            let second_start = self.data_offset.saturating_sub(self.first.len());
            let second_end = end - self.first.len();
            output.extend_from_slice(&self.second[second_start..second_end]);
        }
    }

    fn find_nlattr(&self, start: u32, attr_type: u32, nested: bool) -> u32 {
        const NLA_HEADER_LEN: usize = 4;
        const NLA_TYPE_MASK: u16 = 0x3fff;
        let mut start = match usize::try_from(start) {
            Ok(value) => value,
            Err(_) => return 0,
        };
        if nested {
            let Some(len) = self.load_native_u16(start) else {
                return 0;
            };
            let len = len as usize;
            if len < NLA_HEADER_LEN
                || start
                    .checked_add(len)
                    .is_none_or(|end| end > self.data_len())
            {
                return 0;
            }
            start += NLA_HEADER_LEN;
            return self.find_nlattr_in_range(
                start,
                len - NLA_HEADER_LEN,
                attr_type,
                NLA_TYPE_MASK,
            );
        }
        self.find_nlattr_in_range(
            start,
            self.data_len().saturating_sub(start),
            attr_type,
            NLA_TYPE_MASK,
        )
    }

    fn find_nlattr_in_range(
        &self,
        mut offset: usize,
        length: usize,
        attr_type: u32,
        type_mask: u16,
    ) -> u32 {
        const NLA_HEADER_LEN: usize = 4;
        let Some(end) = offset
            .checked_add(length)
            .filter(|end| *end <= self.data_len())
        else {
            return 0;
        };
        let mut remaining_iterations = length / NLA_HEADER_LEN;
        while remaining_iterations != 0 && offset + NLA_HEADER_LEN <= end {
            remaining_iterations -= 1;
            let Some(nla_len) = self.load_native_u16(offset).map(usize::from) else {
                return 0;
            };
            let Some(nla_type) = self.load_native_u16(offset + 2) else {
                return 0;
            };
            if nla_len < NLA_HEADER_LEN || offset.checked_add(nla_len).is_none_or(|v| v > end) {
                return 0;
            }
            if u32::from(nla_type & type_mask) == attr_type {
                return offset.min(u32::MAX as usize) as u32;
            }
            let aligned = match nla_len.checked_add(3).map(|len| len & !3) {
                Some(value) if value >= NLA_HEADER_LEN => value,
                _ => return 0,
            };
            offset = match offset.checked_add(aligned) {
                Some(value) => value,
                None => return 0,
            };
        }
        0
    }

    fn load_native_u16(&self, data_offset: usize) -> Option<u16> {
        let first = self.byte_at_full(self.data_offset.checked_add(data_offset)?)?;
        let second =
            self.byte_at_full(self.data_offset.checked_add(data_offset)?.checked_add(1)?)?;
        Some(u16::from_ne_bytes([first, second]))
    }

    fn payload_offset(&self) -> u32 {
        if let Some(offset) = self.payload_offset_cache.get() {
            return offset;
        }
        let offset = self.calculate_payload_offset();
        self.payload_offset_cache.set(Some(offset));
        offset
    }

    /// Mirrors Linux 6.6 `skb_get_poff()`: first run the basic flow
    /// dissector, then advance over the small set of L4 headers it knows.
    fn calculate_payload_offset(&self) -> u32 {
        let Some(keys) = self.dissect_flow_keys_basic() else {
            return 0;
        };
        let mut poff = keys
            .thoff
            .saturating_sub(self.data_offset)
            .min(self.data_len())
            .min(u16::MAX as usize)
            .min(u32::MAX as usize) as u32;

        if keys.is_fragment && !keys.is_first_fragment {
            return poff;
        }

        let l4_len = match keys.ip_proto {
            // TCP only needs the doff byte. A short TCP header leaves thoff
            // unchanged, while a declared long header is deliberately not
            // clamped to the packet length.
            6 => {
                let full_offset = self
                    .data_offset
                    .checked_add(poff as usize)
                    .and_then(|offset| offset.checked_add(12));
                let Some(doff) = full_offset.and_then(|offset| self.byte_at_full(offset)) else {
                    return poff;
                };
                u32::from(((doff & 0xf0) >> 2).max(20))
            }
            // UDP, UDPLITE, ICMP, ICMPv6 and IGMP.
            17 | 136 | 1 | 58 | 2 => 8,
            // DCCP and SCTP.
            33 | 132 => 12,
            _ => 0,
        };
        poff = poff.wrapping_add(l4_len);
        poff
    }

    fn dissect_flow_keys_basic(&self) -> Option<BasicFlowKeys> {
        #[derive(Clone, Copy)]
        enum Stage {
            Protocol,
            IpProtocol,
        }

        let mut protocol = self.vlan.map_or(self.protocol, |(_, tpid)| tpid);
        let mut metadata_vlan_pending = self.vlan.is_some();
        let mut ip_proto = 0;
        let mut nhoff = ETHERNET_HEADER_LEN;
        let mut is_fragment = false;
        let mut is_first_fragment = false;
        let mut num_headers = 0u8;
        let mut stage = Stage::Protocol;

        loop {
            let result = match stage {
                Stage::Protocol => self.dissect_protocol(
                    &mut protocol,
                    &mut ip_proto,
                    &mut nhoff,
                    &mut metadata_vlan_pending,
                    &mut is_fragment,
                    &mut is_first_fragment,
                ),
                Stage::IpProtocol => self.dissect_ip_protocol(
                    &mut protocol,
                    &mut ip_proto,
                    &mut nhoff,
                    &mut is_fragment,
                    &mut is_first_fragment,
                ),
            };

            match result {
                FlowDissectRet::Bad => return None,
                FlowDissectRet::StopGood => break,
                FlowDissectRet::Continue => match stage {
                    Stage::Protocol => stage = Stage::IpProtocol,
                    Stage::IpProtocol => break,
                },
                FlowDissectRet::ProtoAgain | FlowDissectRet::IpProtoAgain => {
                    num_headers = num_headers.saturating_add(1);
                    if num_headers > MAX_FLOW_DISSECT_HDRS {
                        break;
                    }
                    stage = if matches!(result, FlowDissectRet::ProtoAgain) {
                        Stage::Protocol
                    } else {
                        Stage::IpProtocol
                    };
                }
            }
        }

        Some(BasicFlowKeys {
            thoff: nhoff.min(self.full_len()),
            ip_proto,
            is_fragment,
            is_first_fragment,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn dissect_protocol(
        &self,
        protocol: &mut u16,
        ip_proto: &mut u8,
        nhoff: &mut usize,
        metadata_vlan_pending: &mut bool,
        is_fragment: &mut bool,
        is_first_fragment: &mut bool,
    ) -> FlowDissectRet {
        const ETH_P_8021Q: u16 = 0x8100;
        const ETH_P_8021AD: u16 = 0x88a8;
        const ETH_P_PPP_SES: u16 = 0x8864;
        const ETH_P_TIPC: u16 = 0x88ca;
        const ETH_P_MPLS_UC: u16 = 0x8847;
        const ETH_P_MPLS_MC: u16 = 0x8848;
        const ETH_P_FCOE: u16 = 0x8906;
        const ETH_P_ARP: u16 = 0x0806;
        const ETH_P_RARP: u16 = 0x8035;
        const ETH_P_BATMAN: u16 = 0x4305;
        const ETH_P_1588: u16 = 0x88f7;
        const ETH_P_PRP: u16 = 0x88fb;
        const ETH_P_HSR: u16 = 0x892f;
        const ETH_P_CFM: u16 = 0x8902;

        match *protocol {
            eth_protocol::ETH_P_IP => {
                if !self.range_present(*nhoff, 20) {
                    return FlowDissectRet::Bad;
                }
                let ihl = usize::from(self.byte_at_full(*nhoff).unwrap() & 0x0f) * 4;
                if ihl < 20 {
                    return FlowDissectRet::Bad;
                }
                *ip_proto = self.byte_at_full(*nhoff + 9).unwrap();
                let frag = self.read_be_u16(*nhoff + 6).unwrap();
                *nhoff = match nhoff.checked_add(ihl) {
                    Some(offset) => offset,
                    None => return FlowDissectRet::Bad,
                };
                if frag & 0x3fff != 0 {
                    *is_fragment = true;
                    *is_first_fragment = frag & 0x1fff == 0;
                    FlowDissectRet::StopGood
                } else {
                    FlowDissectRet::Continue
                }
            }
            eth_protocol::ETH_P_IPV6 => {
                if !self.range_present(*nhoff, 40) {
                    return FlowDissectRet::Bad;
                }
                *ip_proto = self.byte_at_full(*nhoff + 6).unwrap();
                *nhoff = match nhoff.checked_add(40) {
                    Some(offset) => offset,
                    None => return FlowDissectRet::Bad,
                };
                FlowDissectRet::Continue
            }
            ETH_P_8021Q | ETH_P_8021AD => {
                if *metadata_vlan_pending {
                    *metadata_vlan_pending = false;
                    *protocol = self.protocol;
                } else {
                    if !self.range_present(*nhoff, 4) {
                        return FlowDissectRet::Bad;
                    }
                    *protocol = self.read_be_u16(*nhoff + 2).unwrap();
                    *nhoff += 4;
                }
                FlowDissectRet::ProtoAgain
            }
            ETH_P_PPP_SES => self.dissect_pppoe(protocol, nhoff),
            ETH_P_TIPC => {
                if self.range_present(*nhoff, 16) {
                    FlowDissectRet::StopGood
                } else {
                    FlowDissectRet::Bad
                }
            }
            ETH_P_MPLS_UC | ETH_P_MPLS_MC => {
                // The basic dissector does not request the MPLS key, so Linux
                // does not read or validate the LSE and advances exactly one.
                *nhoff = match nhoff.checked_add(4) {
                    Some(offset) => offset,
                    None => return FlowDissectRet::Bad,
                };
                FlowDissectRet::StopGood
            }
            ETH_P_FCOE => {
                if !self.range_present(*nhoff, 38) {
                    return FlowDissectRet::Bad;
                }
                *nhoff += 38;
                FlowDissectRet::StopGood
            }
            ETH_P_ARP | ETH_P_RARP | ETH_P_CFM => FlowDissectRet::StopGood,
            ETH_P_BATMAN => {
                const BATADV_UNICAST_LEN: usize = 10;
                const BATADV_AND_ETH_LEN: usize = BATADV_UNICAST_LEN + ETHERNET_HEADER_LEN;
                if !self.range_present(*nhoff, BATADV_AND_ETH_LEN)
                    || self.byte_at_full(*nhoff) != Some(0x40)
                    || self.byte_at_full(*nhoff + 1) != Some(15)
                {
                    return FlowDissectRet::Bad;
                }
                *protocol = self.read_be_u16(*nhoff + BATADV_UNICAST_LEN + 12).unwrap();
                *nhoff += BATADV_AND_ETH_LEN;
                FlowDissectRet::ProtoAgain
            }
            ETH_P_1588 => {
                const PTP_HEADER_LEN: usize = 34;
                if !self.range_present(*nhoff, PTP_HEADER_LEN) {
                    return FlowDissectRet::Bad;
                }
                *nhoff += PTP_HEADER_LEN;
                FlowDissectRet::StopGood
            }
            ETH_P_PRP | ETH_P_HSR => {
                const HSR_HEADER_LEN: usize = 6;
                if !self.range_present(*nhoff, HSR_HEADER_LEN) {
                    return FlowDissectRet::Bad;
                }
                *protocol = self.read_be_u16(*nhoff + 4).unwrap();
                *nhoff += HSR_HEADER_LEN;
                FlowDissectRet::ProtoAgain
            }
            _ => FlowDissectRet::Bad,
        }
    }

    fn dissect_pppoe(&self, protocol: &mut u16, nhoff: &mut usize) -> FlowDissectRet {
        const PPP_IP: u16 = 0x0021;
        const PPP_IPV6: u16 = 0x0057;
        const PPP_MPLS_UC: u16 = 0x0281;
        const PPP_MPLS_MC: u16 = 0x0283;

        if !self.range_present(*nhoff, 8)
            || self.byte_at_full(*nhoff) != Some(0x11)
            || self.byte_at_full(*nhoff + 1) != Some(0)
        {
            return FlowDissectRet::Bad;
        }
        let mut ppp_proto = self.read_be_u16(*nhoff + 6).unwrap();
        let header_len = if ppp_proto & 0x0100 != 0 {
            ppp_proto >>= 8;
            7
        } else {
            8
        };
        *nhoff += header_len;
        match ppp_proto {
            PPP_IP => {
                *protocol = eth_protocol::ETH_P_IP;
                FlowDissectRet::ProtoAgain
            }
            PPP_IPV6 => {
                *protocol = eth_protocol::ETH_P_IPV6;
                FlowDissectRet::ProtoAgain
            }
            PPP_MPLS_UC => {
                *protocol = 0x8847;
                FlowDissectRet::ProtoAgain
            }
            PPP_MPLS_MC => {
                *protocol = 0x8848;
                FlowDissectRet::ProtoAgain
            }
            value if value & 0x0101 == 0x0001 => FlowDissectRet::StopGood,
            _ => FlowDissectRet::Bad,
        }
    }

    fn dissect_ip_protocol(
        &self,
        protocol: &mut u16,
        ip_proto: &mut u8,
        nhoff: &mut usize,
        is_fragment: &mut bool,
        is_first_fragment: &mut bool,
    ) -> FlowDissectRet {
        match *ip_proto {
            47 => self.dissect_gre(protocol, nhoff),
            0 | 43 | 60 if *protocol == eth_protocol::ETH_P_IPV6 => {
                if !self.range_present(*nhoff, 2) {
                    return FlowDissectRet::Bad;
                }
                *ip_proto = self.byte_at_full(*nhoff).unwrap();
                let header_len = (usize::from(self.byte_at_full(*nhoff + 1).unwrap()) + 1) * 8;
                *nhoff = match nhoff.checked_add(header_len) {
                    Some(offset) => offset,
                    None => return FlowDissectRet::Bad,
                };
                FlowDissectRet::IpProtoAgain
            }
            44 if *protocol == eth_protocol::ETH_P_IPV6 => {
                if !self.range_present(*nhoff, 8) {
                    return FlowDissectRet::Bad;
                }
                *ip_proto = self.byte_at_full(*nhoff).unwrap();
                let frag = self.read_be_u16(*nhoff + 2).unwrap();
                *nhoff += 8;
                *is_fragment = true;
                *is_first_fragment = frag & 0xfff8 == 0;
                FlowDissectRet::StopGood
            }
            4 => {
                *protocol = eth_protocol::ETH_P_IP;
                FlowDissectRet::ProtoAgain
            }
            41 => {
                *protocol = eth_protocol::ETH_P_IPV6;
                FlowDissectRet::ProtoAgain
            }
            137 => {
                *protocol = 0x8847;
                FlowDissectRet::ProtoAgain
            }
            _ => FlowDissectRet::Continue,
        }
    }

    fn dissect_gre(&self, protocol: &mut u16, nhoff: &mut usize) -> FlowDissectRet {
        const GRE_CSUM: u16 = 0x8000;
        const GRE_ROUTING: u16 = 0x4000;
        const GRE_KEY: u16 = 0x2000;
        const GRE_SEQ: u16 = 0x1000;
        const GRE_ACK: u16 = 0x0080;
        const GRE_VERSION: u16 = 0x0007;
        const GRE_PROTO_PPP: u16 = 0x880b;
        const ETH_P_TEB: u16 = 0x6558;

        if !self.range_present(*nhoff, 4) {
            return FlowDissectRet::Bad;
        }
        let flags = self.read_be_u16(*nhoff).unwrap();
        let version = flags & GRE_VERSION;
        if flags & GRE_ROUTING != 0 || version > 1 {
            return FlowDissectRet::StopGood;
        }
        *protocol = self.read_be_u16(*nhoff + 2).unwrap();
        if version == 1 && (*protocol != GRE_PROTO_PPP || flags & GRE_KEY == 0) {
            return FlowDissectRet::StopGood;
        }

        let mut offset = 4usize;
        if flags & GRE_CSUM != 0 {
            offset += 4;
        }
        if flags & GRE_KEY != 0 {
            if !self.range_present(*nhoff + offset, 4) {
                return FlowDissectRet::Bad;
            }
            offset += 4;
        }
        if flags & GRE_SEQ != 0 {
            offset += 4;
        }

        if version == 0 {
            if *protocol == ETH_P_TEB {
                if !self.range_present(*nhoff + offset, ETHERNET_HEADER_LEN) {
                    return FlowDissectRet::Bad;
                }
                *protocol = self.read_be_u16(*nhoff + offset + 12).unwrap();
                offset += ETHERNET_HEADER_LEN;
            }
        } else {
            if flags & GRE_ACK != 0 {
                offset += 4;
            }
            if !self.range_present(*nhoff + offset, 4) {
                return FlowDissectRet::Bad;
            }
            match self.read_be_u16(*nhoff + offset + 2).unwrap() {
                0x0021 => *protocol = eth_protocol::ETH_P_IP,
                0x0057 => *protocol = eth_protocol::ETH_P_IPV6,
                _ => {}
            }
            offset += 4;
        }

        *nhoff = match nhoff.checked_add(offset) {
            Some(offset) => offset,
            None => return FlowDissectRet::Bad,
        };
        FlowDissectRet::ProtoAgain
    }

    #[inline]
    fn range_present(&self, offset: usize, len: usize) -> bool {
        offset
            .checked_add(len)
            .is_some_and(|end| end <= self.full_len())
    }

    #[inline]
    fn read_be_u16(&self, offset: usize) -> Option<u16> {
        Some(u16::from_be_bytes([
            self.byte_at_full(offset)?,
            self.byte_at_full(offset.checked_add(1)?)?,
        ]))
    }
}

impl ClassicBpfInput for PacketFilterInput<'_> {
    fn len(&self) -> u32 {
        self.data_len().min(u32::MAX as usize) as u32
    }

    fn load(&self, offset: i32, width: BpfWidth) -> Option<u32> {
        self.load_full(self.resolve_offset(offset)?, width)
    }

    fn load_ancillary(&self, extension: u32, accumulator: u32, index: u32) -> Option<u32> {
        Some(match extension {
            SKF_AD_PROTOCOL => self.protocol as u32,
            SKF_AD_PKTTYPE => self.ingress.pkt_type as u32,
            SKF_AD_IFINDEX => self.ingress.ifindex,
            SKF_AD_NLATTR => self.find_nlattr(accumulator, index, false),
            SKF_AD_NLATTR_NEST => self.find_nlattr(accumulator, index, true),
            SKF_AD_MARK | SKF_AD_QUEUE | SKF_AD_RXHASH => 0,
            SKF_AD_HATYPE => self.ingress.hatype as u32,
            SKF_AD_CPU => crate::smp::core::smp_get_processor_id().data(),
            SKF_AD_VLAN_TAG => self.vlan.map_or(0, |value| value.0 as u32),
            SKF_AD_VLAN_TAG_PRESENT => u32::from(self.vlan.is_some()),
            SKF_AD_PAY_OFFSET => self.payload_offset(),
            SKF_AD_RANDOM => crate::arch::rand::rand() as u32,
            SKF_AD_VLAN_TPID => self.vlan.map_or(0, |value| value.1 as u32),
            _ => return None,
        })
    }
}

fn parse_frame(frame: &[u8]) -> Option<ParsedFrame> {
    if frame.len() < 14 {
        return None;
    }
    let dst = frame[0..6].try_into().ok()?;
    let src = frame[6..12].try_into().ok()?;
    let outer = u16::from_be_bytes([frame[12], frame[13]]);
    if outer == 0x8100 || outer == 0x88a8 {
        if frame.len() < 18 {
            return None;
        }
        let tci = u16::from_be_bytes([frame[14], frame[15]]);
        let protocol = u16::from_be_bytes([frame[16], frame[17]]);
        Some(ParsedFrame {
            dst,
            src,
            protocol,
            vlan: Some((tci, outer)),
        })
    } else {
        Some(ParsedFrame {
            dst,
            src,
            protocol: outer,
            vlan: None,
        })
    }
}

impl PacketSocket {
    pub(super) fn deliver_from_iface(&self, ingress: PacketIngressMetadata, frame: &[u8]) {
        let (bound_ifindex, bound_protocol) = self.binding.load();
        if bound_protocol == 0 || (bound_ifindex != 0 && bound_ifindex != ingress.ifindex) {
            return;
        }
        let Some(parsed) = parse_frame(frame) else {
            return;
        };
        let input = PacketFilterInput::new(frame, &parsed, self.sock_type, ingress);
        if bound_protocol != eth_protocol::ETH_P_ALL && bound_protocol != input.protocol {
            return;
        }
        let wire_len = input.data_len();
        let filter_result = self
            .filter
            .load()
            .map(|prog| crate::bpf::classic::run_cbpf(&prog, &input));
        let data_len = match filter_result {
            Some(0) => return,
            Some(snaplen) => wire_len.min(snaplen as usize),
            None => wire_len,
        };
        // Linux runs the socket filter before checking sk_rmem_alloc: a packet
        // rejected by cBPF is not counted as a receive-buffer drop. Keep this
        // cheap precheck after the filter and the atomic reservation below as
        // the final concurrent admission check.
        if self.rx_buffer_bytes.load(Ordering::Acquire)
            >= self.recv_buffer_bytes.load(Ordering::Relaxed)
        {
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let metadata = PacketMetadata {
            src_mac: parsed.src,
            dst_mac: parsed.dst,
            protocol: input.protocol,
            ifindex: ingress.ifindex,
            hatype: ingress.hatype,
            pkt_type: ingress.pkt_type,
            // Linux stores origlen after SOCK_DGRAM has advanced data to the
            // network header; without a packet filter, origlen equals the
            // queued visible length for both RAW and DGRAM sockets.
            wire_len,
            mac_offset: 0,
            net_offset: input.network_offset,
            vlan_tci: input.vlan.map_or(0, |v| v.0),
            vlan_tpid: input.vlan.map_or(0, |v| v.1),
        };
        let accounted_bytes = data_len.saturating_add(core::mem::size_of::<ReceivedPacket>());
        if self
            .rx_buffer_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                // Linux checks sk_rmem_alloc before charging the next skb, so
                // one final packet may take the queue over sk_rcvbuf.
                (used < self.recv_buffer_bytes.load(Ordering::Relaxed))
                    .then(|| used.checked_add(accounted_bytes))
                    .flatten()
            })
            .is_err()
        {
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut data = Vec::new();
        if data.try_reserve_exact(data_len).is_err() {
            self.rx_buffer_bytes
                .fetch_sub(accounted_bytes, Ordering::AcqRel);
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        input.copy_prefix_to(&mut data, data_len);
        let packet = ReceivedPacket {
            data,
            metadata,
            accounted_bytes,
        };
        let mut queue = self.rx_buffer.lock();
        if queue.try_reserve(1).is_err() {
            self.rx_buffer_bytes
                .fetch_sub(accounted_bytes, Ordering::AcqRel);
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.push_back(packet);
        drop(queue);
        self.stats_packets.fetch_add(1, Ordering::Relaxed);
        self.wait_queue.wakeup(None);
    }
    pub(super) fn can_recv(&self) -> bool {
        !self.rx_buffer.lock().is_empty()
    }
    fn dequeue(&self, peek: bool) -> Result<ReceivedPacket, SystemError> {
        let mut queue = self.rx_buffer.lock();
        let packet = if peek {
            queue.front().cloned()
        } else {
            queue.pop_front()
        }
        .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        drop(queue);
        if !peek {
            self.rx_buffer_bytes
                .fetch_sub(packet.accounted_bytes, Ordering::AcqRel);
        }
        Ok(packet)
    }
    fn wait_dequeue(&self, flags: PMSG) -> Result<ReceivedPacket, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let peek = flags.contains(PMSG::PEEK);
        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            return self.dequeue(peek);
        }
        if let Some(timeout_ticks) = self.recv_timeout_ticks() {
            self.wait_queue
                .wait_until_timeout_ticks(|| self.dequeue(peek).ok(), timeout_ticks)
        } else {
            self.wait_queue
                .wait_until_interruptible(|| self.dequeue(peek).ok())
        }
    }
    pub(super) fn recv_packet(&self, buf: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        let packet = self.wait_dequeue(flags)?;
        let n = buf.len().min(packet.data.len());
        buf[..n].copy_from_slice(&packet.data[..n]);
        Ok(if flags.contains(PMSG::TRUNC) {
            packet.data.len()
        } else {
            n
        })
    }
    pub(super) fn recv_packet_from(
        &self,
        buf: &mut [u8],
        flags: PMSG,
    ) -> Result<(usize, Endpoint), SystemError> {
        let packet = self.wait_dequeue(flags)?;
        let n = buf.len().min(packet.data.len());
        buf[..n].copy_from_slice(&packet.data[..n]);
        let mut ll = LinkLayerEndpoint::new(packet.metadata.ifindex as usize);
        ll.addr[..6].copy_from_slice(&packet.metadata.src_mac);
        ll.protocol = packet.metadata.protocol;
        ll.hatype = packet.metadata.hatype;
        ll.pkttype = packet.metadata.pkt_type as u8;
        ll.halen = 6;
        Ok((
            if flags.contains(PMSG::TRUNC) {
                packet.data.len()
            } else {
                n
            },
            Endpoint::LinkLayer(ll),
        ))
    }
    pub(super) fn recv_packet_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let capacity = iovs.total_len();
        if capacity == usize::MAX {
            return Err(SystemError::EINVAL);
        }
        let packet = self.wait_dequeue(flags)?;
        let copy_len = capacity.min(packet.data.len());
        let written = iovs.scatter(&packet.data[..copy_len])?;
        if written != copy_len {
            return Err(SystemError::EFAULT);
        }
        if !msg.msg_name.is_null() {
            let full = core::mem::size_of::<SockAddrLl>();
            let n = (msg.msg_namelen as usize).min(full);
            let mut addr = [0; 8];
            addr[..6].copy_from_slice(&packet.metadata.src_mac);
            let sll = SockAddrLl {
                sll_family: 17,
                sll_protocol: packet.metadata.protocol.to_be(),
                sll_ifindex: packet.metadata.ifindex as i32,
                sll_hatype: packet.metadata.hatype,
                sll_pkttype: packet.metadata.pkt_type as u8,
                sll_halen: 6,
                sll_addr: addr,
            };
            let bytes = unsafe { core::slice::from_raw_parts(&sll as *const _ as *const u8, full) };
            let mut w = crate::syscall::user_access::UserBufferWriter::new(
                msg.msg_name as *mut u8,
                n,
                true,
            )?;
            w.buffer_protected(0)?.write_to_user(0, &bytes[..n])?;
            msg.msg_namelen = full as u32;
        } else {
            msg.msg_namelen = 0;
        }
        let control_len = msg.msg_controllen;
        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        if packet.data.len() > capacity {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }
        if self.options.read().auxdata {
            let vlan_status = if packet.metadata.vlan_tpid != 0 {
                TP_STATUS_VLAN_VALID | TP_STATUS_VLAN_TPID_VALID
            } else {
                0
            };
            let aux = TpacketAuxdata {
                tp_status: TP_STATUS_USER | vlan_status,
                tp_len: packet.metadata.wire_len.min(u32::MAX as usize) as u32,
                tp_snaplen: packet.data.len().min(u32::MAX as usize) as u32,
                tp_mac: 0,
                tp_net: if self.sock_type == PacketSocketType::Raw {
                    packet.metadata.net_offset.min(u16::MAX as usize) as u16
                } else {
                    0
                },
                tp_vlan_tci: packet.metadata.vlan_tci,
                tp_vlan_tpid: packet.metadata.vlan_tpid,
            };
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    &aux as *const _ as *const u8,
                    core::mem::size_of::<TpacketAuxdata>(),
                )
            };
            let mut off = 0;
            CmsgBuffer {
                ptr: msg.msg_control,
                len: control_len,
                write_off: &mut off,
            }
            .put(
                &mut msg.msg_flags,
                SOL_PACKET,
                packet_option::PACKET_AUXDATA as i32,
                bytes.len(),
                bytes,
            )?;
            msg.msg_controllen = off;
        }
        Ok(if flags.contains(PMSG::TRUNC) {
            packet.data.len()
        } else {
            copy_len
        })
    }
}
