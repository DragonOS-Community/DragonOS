use crate::{
    driver::net::{types::InterfaceType, Iface},
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kernel::utils::finish_response,
                message::{
                    attr::link::LinkAttr,
                    segment::{
                        link::{LinkMessageFlags, LinkSegment, LinkSegmentBody},
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
use core::num::NonZero;
use system_error::SystemError;

pub(super) fn do_get_link(
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let filter_by = FilterBy::from_requset(request_segment)?;

    let mut responce: Vec<RouteNlSegment> = netns
        .device_list()
        .iter()
        .filter(|(_, iface)| match &filter_by {
            FilterBy::Index(index) => *index == iface.nic_id() as u32,
            FilterBy::Name(name) => *name == iface.name(),
            FilterBy::Dump => true,
        })
        .map(|(_, iface)| iface_to_new_link(request_segment.header(), iface))
        .map(RouteNlSegment::NewLink)
        .collect();

    let dump_all = matches!(filter_by, FilterBy::Dump);

    if !dump_all && responce.is_empty() {
        log::error!("no such device");
        return Err(SystemError::ENODEV);
    }

    finish_response(request_segment.header(), dump_all, &mut responce);

    Ok(responce)
}

enum FilterBy<'a> {
    Index(u32),
    Name(&'a str),
    Dump,
}

impl<'a> FilterBy<'a> {
    fn from_requset(request_segment: &'a LinkSegment) -> Result<Self, SystemError> {
        let dump_all = {
            let flags = GetRequestFlags::from_bits_truncate(request_segment.header().flags);
            flags.contains(GetRequestFlags::DUMP)
        };
        if dump_all {
            validate_dumplink_request(request_segment.body())?;
            return Ok(Self::Dump);
        }

        validate_getlink_request(request_segment.body())?;

        if let Some(required_index) = request_segment.body().index {
            return Ok(Self::Index(required_index.get()));
        }

        let required_name = request_segment.attrs().iter().find_map(|attr| {
            if let LinkAttr::Name(name) = attr {
                Some(name.to_str().ok()?)
            } else {
                None
            }
        });

        if let Some(name) = required_name {
            return Ok(Self::Name(name));
        }

        log::error!("either interface name or index should be specified for non-dump mode");
        Err(SystemError::EINVAL)
    }
}

fn validate_getlink_request(body: &LinkSegmentBody) -> Result<(), SystemError> {
    if !body.flags.is_empty()
        || body.type_ != InterfaceType::NETROM
        || body.pad.is_some()
        || !body.change.is_empty()
    {
        log::error!("the flags or the type is not valid");
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

fn validate_dumplink_request(body: &LinkSegmentBody) -> Result<(), SystemError> {
    // <https://elixir.bootlin.com/linux/v6.13/source/net/core/rtnetlink.c#L2378>.
    if !body.flags.is_empty()
        || body.type_ != InterfaceType::NETROM
        || body.pad.is_some()
        || !body.change.is_empty()
    {
        log::error!("the flags or the type is not valid");
        return Err(SystemError::EINVAL);
    }

    //  <https://elixir.bootlin.com/linux/v6.13/source/net/core/rtnetlink.c#L2383>.
    if body.index.is_some() {
        log::error!("filtering by interface index is not valid for link dumps");
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

fn iface_to_new_link(request_header: &CMsgSegHdr, iface: &Arc<dyn Iface>) -> LinkSegment {
    let header = CMsgSegHdr {
        len: 0,
        type_: CSegmentType::NEWLINK as _,
        flags: SegHdrCommonFlags::empty().bits(),
        seq: request_header.seq,
        pid: request_header.pid,
    };

    let link_message = LinkSegmentBody {
        family: AddressFamily::Unspecified,
        type_: iface.type_(),
        index: NonZero::new(iface.nic_id() as u32),
        flags: iface.flags(),
        change: LinkMessageFlags::empty(),
        pad: None,
    };

    let attrs = vec![
        LinkAttr::Name(CString::new(iface.name()).unwrap()),
        LinkAttr::Mtu(iface.mtu() as u32),
    ];

    LinkSegment::new(header, link_message, attrs)
}
