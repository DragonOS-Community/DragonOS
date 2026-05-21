use crate::{
    driver::net::{
        types::{InterfaceFlags, InterfaceType},
        Iface, Operstate,
    },
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::{
                    finish_response, kernel_notify_header, multicast_notify, RTMGRP_LINK,
                },
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
        .map(|(_, iface)| {
            iface_to_link_message(request_segment.header(), CSegmentType::NEWLINK, iface)
                .map(RouteNlSegment::NewLink)
        })
        .collect::<Result<Vec<_>, _>>()?;

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
    // Linux 对 RTM_GETLINK 不校验 ifi_type/ifi_flags；仅拒绝带 change/pad 的请求。
    if body.pad.is_some() || !body.change.is_empty() {
        log::error!("invalid GETLINK ifinfomsg change/pad");
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

fn validate_dumplink_request(body: &LinkSegmentBody) -> Result<(), SystemError> {
    // <https://elixir.bootlin.com/linux/v6.13/source/net/core/rtnetlink.c#L2383>.
    if body.pad.is_some() || !body.change.is_empty() {
        log::error!("invalid DUMP GETLINK ifinfomsg change/pad");
        return Err(SystemError::EINVAL);
    }

    if body.index.is_some() {
        log::error!("filtering by interface index is not valid for link dumps");
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

fn iface_to_link_message(
    request_header: &CMsgSegHdr,
    msg_type: CSegmentType,
    iface: &Arc<dyn Iface>,
) -> Result<LinkSegment, SystemError> {
    let header = CMsgSegHdr {
        len: 0,
        type_: msg_type as _,
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
        LinkAttr::Address(iface.mac().as_bytes().to_vec()),
        LinkAttr::Name(CString::new(iface.name()).map_err(|_| SystemError::EINVAL)?),
        LinkAttr::Mtu(iface.mtu() as u32),
    ];

    Ok(LinkSegment::new(header, link_message, attrs))
}

pub(super) fn do_del_link(
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let iface = find_iface_for_setlink(request_segment, netns)?;
    if iface.type_() == InterfaceType::LOOPBACK {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
}

pub(super) fn do_set_link(
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let iface = find_iface_for_setlink(request_segment, netns.clone())?;
    let updates = validate_setlink_request(request_segment, iface.as_ref())?;

    if let Some(ref name) = updates.name {
        let duplicate = netns
            .device_list()
            .iter()
            .any(|(_, other)| !Arc::ptr_eq(other, &iface) && other.name() == *name);
        if duplicate {
            return Err(SystemError::EEXIST);
        }
    }

    let current_flags = iface.flags();
    let change_mask = InterfaceFlags::from_bits_truncate(request_segment.body().change.bits());
    let requested_flags = InterfaceFlags::from_bits_truncate(request_segment.body().flags.bits());
    let new_flags = InterfaceFlags::from_bits_truncate(
        (current_flags.bits() & !change_mask.bits())
            | (requested_flags.bits() & change_mask.bits()),
    );

    iface.common().set_flags(new_flags);

    if change_mask.contains(InterfaceFlags::UP) {
        let operstate = if new_flags.contains(InterfaceFlags::UP) {
            Operstate::IF_OPER_UP
        } else {
            Operstate::IF_OPER_DOWN
        };
        iface.set_operstate(operstate);
    }

    if let Some(name) = updates.name {
        iface.set_name(name);
    }

    if let Some(mtu) = updates.mtu {
        iface.common().set_mtu(mtu as usize);
    }

    multicast_notify(
        netns,
        RTMGRP_LINK,
        RouteNlSegment::NewLink(iface_to_link_message(
            &kernel_notify_header(CSegmentType::NEWLINK),
            CSegmentType::NEWLINK,
            &iface,
        )?),
    );

    Ok(Vec::new())
}

fn find_iface_for_setlink(
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Arc<dyn Iface>, SystemError> {
    if let Some(index) = request_segment.body().index {
        return netns
            .device_list()
            .get(&(index.get() as usize))
            .cloned()
            .ok_or(SystemError::ENODEV);
    }

    let requested_name = request_segment.attrs().iter().find_map(|attr| {
        if let LinkAttr::Name(name) = attr {
            name.to_str().ok()
        } else {
            None
        }
    });

    if let Some(name) = requested_name {
        return netns
            .device_list()
            .iter()
            .find(|(_, iface)| iface.name() == name)
            .map(|(_, iface)| iface.clone())
            .ok_or(SystemError::ENODEV);
    }

    Err(SystemError::EINVAL)
}

struct SetLinkUpdates {
    name: Option<alloc::string::String>,
    mtu: Option<u32>,
}

fn validate_setlink_request(
    request_segment: &LinkSegment,
    iface: &dyn Iface,
) -> Result<SetLinkUpdates, SystemError> {
    let body = request_segment.body();
    if body.pad.is_some() {
        return Err(SystemError::EINVAL);
    }

    let mut updates = SetLinkUpdates {
        name: None,
        mtu: None,
    };
    for attr in request_segment.attrs() {
        match attr {
            LinkAttr::Name(name) => {
                let name =
                    alloc::string::String::from(name.to_str().map_err(|_| SystemError::EINVAL)?);
                if name.is_empty() {
                    return Err(SystemError::EINVAL);
                }
                if name != iface.name() {
                    updates.name = Some(name);
                }
            }
            LinkAttr::Mtu(mtu) => {
                if *mtu == 0 {
                    return Err(SystemError::EINVAL);
                }
                if *mtu != iface.mtu() as u32 {
                    updates.mtu = Some(*mtu);
                }
            }
            LinkAttr::TxqLen(_) | LinkAttr::LinkMode(_) | LinkAttr::ExtMask(_) => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }

    Ok(updates)
}
