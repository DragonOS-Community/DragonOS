use crate::{
    driver::net::{
        types::{InterfaceFlags, InterfaceType},
        Iface,
    },
    net::link::{LinkFlagsUpdate, LinkMtuUpdate, LinkMutationCommit, LinkTarget, LinkUpdate},
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
use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
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

    finish_response(request_segment.header(), dump_all, &mut responce)?;

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
    let flags = iface.common().link_flags_snapshot()?;
    let user_visible_flags = iface.project_user_visible_flags(flags.configured);
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
        flags: user_visible_flags,
        change: LinkMessageFlags::empty(),
        pad: None,
    };

    let attrs = vec![
        LinkAttr::Address(iface.mac().as_bytes().to_vec()),
        LinkAttr::Name(CString::new(iface.name()).map_err(|_| SystemError::EINVAL)?),
        LinkAttr::Mtu(iface.mtu() as u32),
        LinkAttr::Promiscuity(flags.promiscuity),
        LinkAttr::Allmulti(flags.allmulti),
    ];

    Ok(LinkSegment::new(header, link_message, attrs))
}

pub(crate) fn notify_link_change(iface: &Arc<dyn Iface>) {
    let Some(netns) = iface.net_namespace() else {
        return;
    };
    let segment = iface_to_link_message(
        &kernel_notify_header(CSegmentType::NEWLINK),
        CSegmentType::NEWLINK,
        iface,
    );
    match segment {
        Ok(segment) => multicast_notify(netns, RTMGRP_LINK, RouteNlSegment::NewLink(segment)),
        Err(err) => log::warn!(
            "netlink route: failed to build link notification: {:?}",
            err
        ),
    }
}

pub(super) fn do_del_link(
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let iface = find_iface_for_link(request_segment, &netns)?;
    if iface.type_() == InterfaceType::LOOPBACK {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
}

fn find_iface_for_link(
    request_segment: &LinkSegment,
    netns: &Arc<NetNamespace>,
) -> Result<Arc<dyn Iface>, SystemError> {
    if let Some(index) = request_segment.body().index {
        return netns
            .device_list()
            .get(&(index.get() as usize))
            .cloned()
            .ok_or(SystemError::ENODEV);
    }
    let name = request_segment.attrs().iter().find_map(|attr| match attr {
        LinkAttr::Name(name) => name.to_str().ok(),
        _ => None,
    });
    let name = name.ok_or(SystemError::EINVAL)?;
    netns
        .device_list()
        .values()
        .find(|iface| iface.common().with_iface_name(|current| current == name))
        .cloned()
        .ok_or(SystemError::ENODEV)
}

pub(super) fn do_set_link(
    rtnl: &crate::net::rtnl::RtnlGuard,
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let (target, update) = parse_setlink_request(request_segment)?;
    let committed = crate::net::link::mutate_link(rtnl, &netns, target, update)?;
    notify_link_commit(&netns, committed);
    Ok(Vec::new())
}

pub(crate) fn notify_link_commit(netns: &Arc<NetNamespace>, committed: LinkMutationCommit) {
    let LinkMutationCommit {
        iface,
        changes,
        renamed_ipv4,
        route_changes,
        removed_neighbors,
        rename_old_devpath,
    } = committed;
    if let Some(old_devpath) = rename_old_devpath {
        crate::driver::net::sysfs::netdev_emit_move_uevent(iface.clone(), old_devpath);
    }
    if !changes.is_empty() {
        notify_link_change(&iface);
    }
    for cidr in renamed_ipv4 {
        super::addr::notify_address_change(netns.clone(), &iface, cidr);
    }
    if let Some(changes) = route_changes {
        notify_link_route_changes(netns, changes);
    }
    for entry in removed_neighbors {
        super::neigh::notify_removed_entry(netns, entry);
    }
}

fn notify_link_route_changes(
    netns: &Arc<NetNamespace>,
    changes: crate::net::route::RouteNotifications,
) {
    // Linux 6.6 withdraws IPv4 aliases silently through fib_flush(), while
    // fib6_ifdown emits RTM_DELROUTE. Link-up address-derived routes are regular
    // insertions and emit RTM_NEWROUTE for both families.
    let crate::net::route::RouteNotifications { added, removed } = changes;
    for route in removed {
        super::route::notify_route(netns, CSegmentType::DELROUTE, route);
    }
    for route in added {
        super::route::notify_route(netns, CSegmentType::NEWROUTE, route);
    }
}

fn parse_setlink_request(
    request_segment: &LinkSegment,
) -> Result<(LinkTarget<'_>, LinkUpdate), SystemError> {
    if request_segment.body().pad.is_some() {
        return Err(SystemError::EINVAL);
    }
    // nla_parse() keeps the last attribute of a given type.
    let requested_name = request_segment
        .attrs()
        .iter()
        .filter_map(|attr| {
            if let LinkAttr::Name(name) = attr {
                name.to_str().ok()
            } else {
                None
            }
        })
        .next_back();
    if let Some(index) = request_segment.body().index {
        let mut update = LinkUpdate::default();
        if let Some(name) = requested_name {
            update.new_name = Some(try_string_from_str(name)?);
        }
        parse_setlink_attrs(request_segment, &mut update, true)?;
        update.flags = parse_flags(request_segment);
        return Ok((LinkTarget::Index(index.get()), update));
    }
    let name = requested_name.ok_or(SystemError::EINVAL)?;
    let mut update = LinkUpdate::default();
    parse_setlink_attrs(request_segment, &mut update, false)?;
    update.flags = parse_flags(request_segment);
    Ok((LinkTarget::Name(name), update))
}

fn parse_setlink_attrs(
    request_segment: &LinkSegment,
    update: &mut LinkUpdate,
    name_is_mutation: bool,
) -> Result<(), SystemError> {
    for attr in request_segment.attrs() {
        match attr {
            LinkAttr::Name(name) => {
                name.to_str().map_err(|_| SystemError::EINVAL)?;
                if name_is_mutation {
                    // Already copied above; keep parsing focused on policy.
                }
            }
            LinkAttr::Mtu(mtu) => update.mtu = Some(LinkMtuUpdate::Rtnetlink(*mtu)),
            LinkAttr::Allmulti(_) => return Err(SystemError::EINVAL),
            LinkAttr::Promiscuity(_)
            | LinkAttr::TxqLen(_)
            | LinkAttr::LinkMode(_)
            | LinkAttr::ExtMask(_) => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(())
}

fn parse_flags(request_segment: &LinkSegment) -> Option<LinkFlagsUpdate> {
    let body = request_segment.body();
    if body.flags.is_empty() && body.change.is_empty() {
        return None;
    }
    Some(LinkFlagsUpdate::Masked {
        requested: InterfaceFlags::from_bits_truncate(body.flags.bits()),
        change: InterfaceFlags::from_bits_truncate(body.change.bits()),
    })
}

fn try_string_from_str(source: &str) -> Result<String, SystemError> {
    let mut result = String::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    result.push_str(source);
    Ok(result)
}
