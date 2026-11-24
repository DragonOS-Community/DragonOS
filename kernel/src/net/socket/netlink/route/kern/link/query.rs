use super::*;
use crate::driver::net::Operstate;

pub(crate) fn do_get_link(
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let filter_by = FilterBy::from_requset(request_segment)?;

    let responce: Vec<RouteNlSegment> = netns
        .device_list()
        .iter()
        .filter(|(_, iface)| match &filter_by {
            FilterBy::Index(index) => *index == iface.nic_id() as u32,
            FilterBy::Name(name) => *name == iface.name(),
        })
        .map(|(_, iface)| {
            iface_to_link_message(request_segment.header(), CSegmentType::NEWLINK, iface)
                .map(RouteNlSegment::NewLink)
        })
        .collect::<Result<Vec<_>, _>>()?;

    if responce.is_empty() {
        return Err(SystemError::ENODEV);
    }

    Ok(responce)
}

enum FilterBy<'a> {
    Index(u32),
    Name(&'a str),
}

impl<'a> FilterBy<'a> {
    fn from_requset(request_segment: &'a LinkSegment) -> Result<Self, SystemError> {
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

pub(in crate::net::socket::netlink::route::kern) fn validate_dumplink_request(
    body: &LinkSegmentBody,
) -> Result<(), SystemError> {
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

pub(in crate::net::socket::netlink::route::kern) fn iface_to_link_message(
    request_header: &CMsgSegHdr,
    msg_type: CSegmentType,
    iface: &Arc<dyn Iface>,
) -> Result<LinkSegment, SystemError> {
    let (header, link_message, mut attrs) = link_message_parts(request_header, msg_type, iface)?;
    let kind = if iface.as_any_ref().is::<BridgeIface>() {
        Some(b"bridge\0".as_slice())
    } else if iface.as_any_ref().is::<VethInterface>() {
        Some(b"veth\0".as_slice())
    } else {
        None
    };
    if let Some(kind) = kind {
        let mut info = Vec::new();
        append_nested_link_attr(&mut info, 1, kind)?;
        if kind == b"bridge\0" {
            bridge_info::append_settings(&mut info)?;
        }
        attrs.push(LinkAttr::LinkInfo(info));
    }
    if let Some(veth) = iface.as_any_ref().downcast_ref::<VethInterface>() {
        if let Some(master) = veth.bridge_master() {
            attrs.push(LinkAttr::Master(master.nic_id() as u32));
        }
        if let Some(peer) = veth.peer_veth_opt() {
            if let (Some(owner), Some(peer_owner)) = (iface.net_namespace(), peer.net_namespace()) {
                if !Arc::ptr_eq(&owner, &peer_owner) {
                    attrs.push(LinkAttr::LinkNetnsid(
                        owner.peer_netnsid(&peer_owner)? as u32
                    ));
                }
            }
            attrs.push(LinkAttr::Link(peer.nic_id() as u32));
        }
    }

    Ok(LinkSegment::new(header, link_message, attrs))
}

pub(super) fn link_message_parts(
    request_header: &CMsgSegHdr,
    msg_type: CSegmentType,
    iface: &Arc<dyn Iface>,
) -> Result<(CMsgSegHdr, LinkSegmentBody, Vec<LinkAttr>), SystemError> {
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

    // Delete messages are prepared before shutdown to preserve old identity.
    let operstate = if msg_type == CSegmentType::DELLINK {
        Operstate::IF_OPER_DOWN
    } else {
        iface.user_visible_operstate()
    };
    let attrs = vec![
        LinkAttr::Address(iface.mac().as_bytes().to_vec()),
        LinkAttr::Name(CString::new(iface.name()).map_err(|_| SystemError::EINVAL)?),
        LinkAttr::Mtu(iface.mtu() as u32),
        LinkAttr::TxqLen(iface.common().tx_queue_len()),
        LinkAttr::OperState(operstate as u8),
        LinkAttr::Promiscuity(flags.promiscuity),
        LinkAttr::Allmulti(flags.allmulti),
    ];

    Ok((header, link_message, attrs))
}
