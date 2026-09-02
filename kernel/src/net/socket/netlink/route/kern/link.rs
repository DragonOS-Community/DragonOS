use crate::{
    driver::net::{
        napi::napi_schedule,
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
    let iface = find_iface_for_setlink(request_segment, netns)?;
    if iface.type_() == InterfaceType::LOOPBACK {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
}

pub(super) fn do_set_link(
    rtnl: &crate::net::rtnl::RtnlGuard,
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let iface = find_iface_for_setlink(request_segment, netns.clone())?;
    let updates = validate_setlink_request(request_segment, iface.as_ref())?;
    let committed =
        PreparedSetLink::prepare(rtnl, request_segment, netns.clone(), iface.clone(), updates)?
            .commit();

    notify_link_change(&iface);
    for cidr in committed.renamed_ipv4 {
        super::addr::notify_address_change(netns.clone(), &iface, cidr);
    }
    if let Some(changes) = committed.route_changes {
        notify_link_route_changes(&netns, changes);
    }

    Ok(Vec::new())
}

struct PreparedLinkRename {
    name: String,
    labels: crate::net::address::PreparedAddressLabelRename,
}

/// A SETLINK mutation whose validation and allocations have completed.
///
/// This statically composes the concrete name/address, flag, and route plans;
/// a generic transaction framework would obscure their different publication
/// ordering without improving the single SETLINK call site.
struct PreparedSetLink<'rtnl> {
    _rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
    iface: Arc<dyn Iface>,
    netns: Arc<NetNamespace>,
    mtu: Option<u32>,
    rename: Option<PreparedLinkRename>,
    flags: crate::driver::net::PreparedConfiguredFlags,
    routes: Option<crate::net::route::PreparedLinkStateChange<'rtnl>>,
    changes_up: bool,
    was_up: bool,
    is_up: bool,
}

struct CommittedSetLink {
    renamed_ipv4: Vec<smoltcp::wire::IpCidr>,
    route_changes: Option<crate::net::route::RouteNotifications>,
}

impl<'rtnl> PreparedSetLink<'rtnl> {
    fn prepare(
        rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
        request_segment: &LinkSegment,
        netns: Arc<NetNamespace>,
        iface: Arc<dyn Iface>,
        updates: SetLinkUpdates,
    ) -> Result<Self, SystemError> {
        let rename = if let Some(name) = updates.name {
            let duplicate = netns.device_list().values().any(|other| {
                !Arc::ptr_eq(other, &iface)
                    && other
                        .common()
                        .with_iface_name(|current| current == name.as_str())
            });
            if duplicate {
                return Err(SystemError::EEXIST);
            }
            let labels =
                crate::net::address::PreparedAddressLabelRename::prepare(rtnl, &iface, &name)?;
            Some(PreparedLinkRename { name, labels })
        } else {
            None
        };

        let change_mask = InterfaceFlags::from_bits_truncate(request_segment.body().change.bits());
        let requested_flags =
            InterfaceFlags::from_bits_truncate(request_segment.body().flags.bits());
        let flags = iface
            .common()
            .prepare_configured_flags(requested_flags, change_mask)?;
        let was_up = flags.old_flags().contains(InterfaceFlags::UP);
        let is_up = flags.new_flags().contains(InterfaceFlags::UP);
        let changes_up = change_mask.contains(InterfaceFlags::UP);
        let routes = if changes_up && was_up != is_up {
            Some(crate::net::route::prepare_link_state_change(
                rtnl, &netns, &iface, is_up,
            )?)
        } else {
            None
        };

        Ok(Self {
            _rtnl: rtnl,
            iface,
            netns,
            mtu: updates.mtu,
            rename,
            flags,
            routes,
            changes_up,
            was_up,
            is_up,
        })
    }

    fn commit(self) -> CommittedSetLink {
        let Self {
            _rtnl: _,
            iface,
            netns,
            mtu,
            rename,
            flags,
            routes,
            changes_up,
            was_up,
            is_up,
        } = self;

        // Linux applies MTU and rename before dev_change_flags(). All
        // fallible work is already owned by this plan, so publication cannot
        // strand a half-prepared SETLINK request.
        if let Some(mtu) = mtu {
            iface.common().set_mtu(mtu as usize);
        }
        let renamed_ipv4 = if let Some(rename) = rename {
            rename.labels.publish(&iface, rename.name)
        } else {
            Vec::new()
        };

        let route_changes = if let Some(routes) = routes {
            Some(routes.publish(&netns, is_up, || {
                publish_link_flags_and_state(&iface, flags, is_up);
            }))
        } else {
            // Linux applies an idempotent IFF_UP request to the runtime
            // lifecycle too; only FIB publication requires a transition.
            if changes_up {
                publish_link_flags_and_state(&iface, flags, is_up);
            } else {
                iface.common().publish_configured_flags(flags);
            }
            None
        };

        if changes_up && was_up != is_up {
            if is_up {
                if let Some(napi) = iface.napi_struct() {
                    napi_schedule(napi);
                } else {
                    netns.wakeup_poll_thread();
                }
            }
            netns.notify_deadline_changed();
        }

        CommittedSetLink {
            renamed_ipv4,
            route_changes,
        }
    }
}

fn publish_link_flags_and_state(
    iface: &Arc<dyn Iface>,
    prepared_flags: crate::driver::net::PreparedConfiguredFlags,
    is_up: bool,
) {
    iface.common().publish_configured_flags(prepared_flags);
    if is_up {
        iface.set_operstate(Operstate::IF_OPER_UP);
        iface.set_net_state(crate::driver::net::NetDeivceState::__LINK_STATE_START);
    } else {
        iface.clear_net_state(crate::driver::net::NetDeivceState::__LINK_STATE_START);
        iface.set_operstate(Operstate::IF_OPER_DOWN);
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
            .find(|(_, iface)| iface.common().with_iface_name(|current| current == name))
            .map(|(_, iface)| iface.clone())
            .ok_or(SystemError::ENODEV);
    }

    Err(SystemError::EINVAL)
}

struct SetLinkUpdates {
    name: Option<String>,
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
                let name = try_string_from_str(name.to_str().map_err(|_| SystemError::EINVAL)?)?;
                if name.is_empty() {
                    return Err(SystemError::EINVAL);
                }
                if !iface
                    .common()
                    .with_iface_name(|current| current == name.as_str())
                {
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
            LinkAttr::Allmulti(_) => return Err(SystemError::EINVAL),
            LinkAttr::Promiscuity(_)
            | LinkAttr::TxqLen(_)
            | LinkAttr::LinkMode(_)
            | LinkAttr::ExtMask(_) => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }

    Ok(updates)
}

fn try_string_from_str(source: &str) -> Result<String, SystemError> {
    let mut result = String::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    result.push_str(source);
    Ok(result)
}
