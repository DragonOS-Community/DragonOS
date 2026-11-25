use super::*;
use crate::net::rtnl::RtnlGuard;

const IFLA_BRPORT_MODE: u16 = 4;

pub(super) fn set_hairpin(
    rtnl: &RtnlGuard,
    request: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<(), SystemError> {
    let index = request.body().index.ok_or(SystemError::ENODEV)?;
    let iface = netns
        .device_list()
        .get(&(index.get() as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    let membership = iface
        .bridge_port_data()
        .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;

    let mut protinfo = None;
    for attr in request.attrs() {
        let LinkAttr::ProtInfo { nested, data } = attr else {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        };
        // br_setlink uses nlmsg_find_attr() to select the first PROTINFO.
        protinfo.get_or_insert((*nested, data));
    }
    let Some((nested, data)) = protinfo else {
        return Ok(());
    };
    if !nested {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let mut mode = None;
    for attr in parse_nested_link_attrs(data)? {
        if attr.kind != IFLA_BRPORT_MODE {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        // Linux's deprecated nested NLA_U8 policy permits a longer payload.
        mode = Some(*attr.payload.first().ok_or(SystemError::ERANGE)? != 0);
    }
    let Some(mode) = mode else {
        return Ok(());
    };
    membership.set_hairpin(rtnl, mode)?;

    match port_to_link_message(&kernel_notify_header(CSegmentType::NEWLINK), &iface) {
        Ok(Some(segment)) => multicast_notify(netns, RTMGRP_LINK, RouteNlSegment::NewLink(segment)),
        Ok(None) => {}
        Err(err) => log::warn!(
            "netlink route: failed to build bridge port notification: {:?}",
            err
        ),
    }
    Ok(())
}

pub(in crate::net::socket::netlink::route::kern) fn port_to_link_message(
    request_header: &CMsgSegHdr,
    iface: &Arc<dyn Iface>,
) -> Result<Option<LinkSegment>, SystemError> {
    let Some((master, hairpin)) = iface
        .bridge_port_data()
        .and_then(|membership| membership.hairpin_state())
    else {
        return Ok(None);
    };
    let (header, mut body, mut attrs) =
        query::link_message_parts(request_header, CSegmentType::NEWLINK, iface)?;
    body.family = AddressFamily::Bridge;
    attrs.push(LinkAttr::Master(master));
    let mut data = Vec::new();
    append_nested_link_attr(&mut data, IFLA_BRPORT_MODE, &[u8::from(hairpin)])?;
    attrs.push(LinkAttr::ProtInfo { nested: true, data });
    Ok(Some(LinkSegment::new(header, body, attrs)))
}
