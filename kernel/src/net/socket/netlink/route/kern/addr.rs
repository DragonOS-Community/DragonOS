use crate::{
    driver::net::Iface,
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::finish_response,
                message::{
                    attr::addr::AddrAttr,
                    segment::{
                        addr::{AddrMessageFlags, AddrSegment, AddrSegmentBody, RtScope},
                        RouteNlSegment,
                    },
                },
            },
        },
        AddressFamily,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::ffi::CString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use system_error::SystemError;

pub(super) fn do_get_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let dump_all = {
        let flags = GetRequestFlags::from_bits_truncate(request_segment.header().flags);
        flags.contains(GetRequestFlags::DUMP)
    };

    if !dump_all {
        log::error!("GetAddr request without DUMP flag is not supported yet");
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let mut responce: Vec<RouteNlSegment> = netns
        .device_list()
        .iter()
        .filter_map(|(_, iface)| iface_to_new_addr(request_segment.header(), iface))
        .map(RouteNlSegment::NewAddr)
        .collect();

    finish_response(request_segment.header(), dump_all, &mut responce);

    Ok(responce)
}

fn iface_to_new_addr(request_header: &CMsgSegHdr, iface: &Arc<dyn Iface>) -> Option<AddrSegment> {
    let ipv4_addr = iface.common().ipv4_addr()?;

    let header = CMsgSegHdr {
        len: 0,
        type_: CSegmentType::NEWADDR as _,
        flags: SegHdrCommonFlags::empty().bits(),
        seq: request_header.seq,
        pid: request_header.pid,
    };

    let addr_message = AddrSegmentBody {
        family: AddressFamily::INet as _,
        prefix_len: iface.common().prefix_len().unwrap(),
        flags: AddrMessageFlags::PERMANENT,
        scope: RtScope::HOST,
        index: NonZeroU32::new(iface.nic_id() as u32),
    };

    let attrs = vec![
        AddrAttr::Address(ipv4_addr.octets()),
        AddrAttr::Label(CString::new(iface.iface_name()).unwrap()),
        AddrAttr::Local(ipv4_addr.octets()),
    ];

    Some(AddrSegment::new(header, addr_message, attrs))
}
