use super::*;

fn generated_name(
    netns: &Arc<NetNamespace>,
    prefix: &str,
    avoid: &str,
) -> Result<String, SystemError> {
    for suffix in 0..100_000 {
        let name = alloc::format!("{prefix}{suffix}");
        if name.len() < 16
            && name != avoid
            && !netns
                .device_list()
                .values()
                .any(|iface| iface.name() == name)
        {
            return Ok(name);
        }
    }
    Err(SystemError::ENFILE)
}

#[derive(Default)]
struct InitialLinkSettings {
    mtu: Option<u32>,
    tx_queue_len: Option<u32>,
    mac: Option<EthernetAddress>,
    flags: Option<LinkFlagsUpdate>,
}

fn apply_initial_link_settings(
    rtnl: &crate::net::rtnl::RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    settings: InitialLinkSettings,
) -> Result<(), SystemError> {
    if let Some(mac) = settings.mac {
        if let Some(bridge) = iface.as_any_ref().downcast_ref::<BridgeIface>() {
            bridge.set_mac(mac)?;
        } else if let Some(veth) = iface.as_any_ref().downcast_ref::<VethInterface>() {
            veth.set_mac(mac)?;
        } else {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
    }
    if settings.mtu.is_some() || settings.tx_queue_len.is_some() || settings.flags.is_some() {
        crate::net::link::mutate_link(
            rtnl,
            netns,
            LinkTarget::Index(iface.nic_id() as u32),
            LinkUpdate {
                new_name: None,
                mtu: settings.mtu.map(LinkMtuUpdate::Rtnetlink),
                tx_queue_len: settings.tx_queue_len,
                flags: settings.flags,
            },
        )?;
    }
    Ok(())
}

fn rollback_new_link(
    rtnl: &crate::net::rtnl::RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: Arc<dyn Iface>,
    cause: SystemError,
) -> SystemError {
    // No address or route can have been installed on a link created by this
    // request while RTNL is held. Rollback must not allocate or fail after a
    // partially applied initial setting, so the normal DELLINK path is not
    // suitable here.
    crate::net::link::topology::rollback_new_link_unrouted(rtnl, netns, iface);
    cause
}

struct VethPeer {
    netns: Arc<NetNamespace>,
    name: String,
    settings: InitialLinkSettings,
}

fn veth_peer(
    context: &RtnlRequestContext,
    outer_netns: &Arc<NetNamespace>,
    info_data: Option<&[u8]>,
    outer_name: &str,
) -> Result<VethPeer, SystemError> {
    let mut peer_name = None;
    let mut peer_fd = None;
    let mut peer_pid = None;
    let mut has_peer_attr = false;
    let mut settings = InitialLinkSettings::default();
    if let Some(info_data) = info_data {
        for attr in parse_nested_link_attrs(info_data)? {
            if attr.kind != 1 {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            has_peer_attr = true;
            if attr.payload.len() < size_of::<CIfinfoMsg>() {
                return Err(SystemError::EINVAL);
            }
            // VETH_INFO_PEER begins with ifinfomsg, followed by ordinary IFLAs.
            let ifm =
                unsafe { core::ptr::read_unaligned(attr.payload.as_ptr() as *const CIfinfoMsg) };
            if ifm.index != 0 {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            if ifm.flags != 0 || ifm.change != 0 {
                settings.flags = Some(LinkFlagsUpdate::Masked {
                    requested: InterfaceFlags::from_bits_truncate(ifm.flags),
                    change: InterfaceFlags::from_bits_truncate(ifm.change),
                });
            }
            for peer_attr in parse_nested_link_attrs(&attr.payload[size_of::<CIfinfoMsg>()..])? {
                match peer_attr.kind {
                    3 => peer_name = Some(try_string_from_str(nla_name(peer_attr.payload)?)?),
                    1 => settings.mac = Some(link_mac(peer_attr.payload)?),
                    4 if peer_attr.payload.len() == 4 => {
                        settings.mtu =
                            Some(u32::from_ne_bytes(peer_attr.payload.try_into().unwrap()));
                    }
                    4 => return Err(SystemError::EINVAL),
                    13 if peer_attr.payload.len() == 4 => {
                        settings.tx_queue_len =
                            Some(u32::from_ne_bytes(peer_attr.payload.try_into().unwrap()));
                    }
                    13 => return Err(SystemError::EINVAL),
                    28 if peer_attr.payload.len() == 4 => {
                        peer_fd = Some(u32::from_ne_bytes(peer_attr.payload.try_into().unwrap()));
                    }
                    28 => return Err(SystemError::EINVAL),
                    19 if peer_attr.payload.len() == 4 => {
                        peer_pid = Some(u32::from_ne_bytes(peer_attr.payload.try_into().unwrap()));
                    }
                    19 => return Err(SystemError::EINVAL),
                    _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                }
            }
        }
    }
    // Linux veth_newlink uses the outer attributes when VETH_INFO_PEER is
    // absent; otherwise it uses only the peer attributes (defaulting to
    // src_net when the peer does not specify a namespace).
    let netns = if has_peer_attr {
        target_namespace(context, peer_fd, peer_pid)?
    } else {
        outer_netns.clone()
    };
    let name = match peer_name {
        Some(name) => name,
        None => generated_name(&netns, "veth", outer_name)?,
    };
    Ok(VethPeer {
        netns,
        name,
        settings,
    })
}

pub(crate) fn do_new_link(
    rtnl: &crate::net::rtnl::RtnlGuard,
    request: &LinkSegment,
    context: &RtnlRequestContext,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    if request.body().pad.is_some() {
        return Err(SystemError::EINVAL);
    }
    let flags = NewRequestFlags::from_bits_truncate(request.header().flags);
    let source_netns = context.netns();
    let requested_name = request
        .attrs()
        .iter()
        .rev()
        .find_map(|attr| match attr {
            LinkAttr::Name(name) => Some(name.to_str().map_err(|_| SystemError::EINVAL)),
            _ => None,
        })
        .transpose()?;
    if let Some(name) = requested_name {
        nla_name(name.as_bytes())?;
    }
    let existing = if let Some(index) = request.body().index {
        source_netns
            .device_list()
            .get(&(index.get() as usize))
            .cloned()
    } else {
        requested_name.and_then(|name| {
            source_netns
                .device_list()
                .values()
                .find(|iface| iface.name() == name)
                .cloned()
        })
    };
    if let Some(existing) = existing {
        if flags.contains(NewRequestFlags::EXCL) {
            return Err(SystemError::EEXIST);
        }
        if flags.contains(NewRequestFlags::REPLACE) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if request
            .attrs()
            .iter()
            .any(|attr| matches!(attr, LinkAttr::LinkInfo(_)))
        {
            let (kind, data) = linkinfo(request)?;
            let matching_kind = match kind {
                "bridge" => existing.as_any_ref().is::<BridgeIface>(),
                "veth" => existing.as_any_ref().is::<VethInterface>(),
                _ => false,
            };
            if !matching_kind || data.is_some_and(|data| !data.is_empty()) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }
        return do_set_link(rtnl, request, context);
    }
    if !flags.contains(NewRequestFlags::CREATE) {
        return Err(SystemError::ENODEV);
    }
    if flags.contains(NewRequestFlags::REPLACE) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if request.body().index.is_some() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let mut target_fd = None;
    let mut target_pid = None;
    let mut master_index = None;
    let mut settings = InitialLinkSettings {
        flags: parse_flags(request),
        ..Default::default()
    };
    for attr in request.attrs() {
        match attr {
            LinkAttr::Name(_) | LinkAttr::LinkInfo(_) => {}
            LinkAttr::NetNsFd(fd) => target_fd = Some(*fd),
            LinkAttr::NetNsPid(pid) => target_pid = Some(*pid),
            LinkAttr::Master(index) => master_index = Some(*index),
            LinkAttr::Mtu(mtu) => settings.mtu = Some(*mtu),
            LinkAttr::TxqLen(len) => settings.tx_queue_len = Some(*len),
            LinkAttr::Address(bytes) => settings.mac = Some(link_mac(bytes)?),
            LinkAttr::NewIfIndex(_) => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    let target_netns = target_namespace(context, target_fd, target_pid)?;
    let (kind, info_data) = linkinfo(request)?;
    let name = match requested_name {
        Some(name) => try_string_from_str(name)?,
        None => generated_name(&target_netns, kind, "")?,
    };
    let (iface, mut peer) = match kind {
        "bridge" => {
            if info_data.is_some_and(|data| !data.is_empty()) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            (
                crate::net::link::topology::create_bridge(rtnl, &target_netns, &name)?,
                None,
            )
        }
        "veth" => {
            let peer = veth_peer(context, &target_netns, info_data, &name)?;
            let (iface, peer_iface) = crate::net::link::topology::create_veth(
                rtnl,
                &target_netns,
                &name,
                &peer.netns,
                &peer.name,
            )?;
            (iface, Some((peer_iface, peer.netns, peer.settings)))
        }
        _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
    };
    let configured = if let Some((peer_iface, peer_netns, peer_settings)) = peer.as_mut() {
        apply_initial_link_settings(rtnl, peer_netns, peer_iface, core::mem::take(peer_settings))
    } else {
        Ok(())
    }
    .and_then(|_| apply_initial_link_settings(rtnl, &target_netns, &iface, settings));
    if let Err(error) = configured {
        return Err(rollback_new_link(rtnl, &target_netns, iface, error));
    }
    if let Some(index) = master_index.filter(|index| *index != 0) {
        let master = target_netns.device_list().get(&(index as usize)).cloned();
        let bridge_carrier = master
            .as_ref()
            .and_then(|master| crate::net::link::topology::as_bridge(master.clone()))
            .map(BridgeCarrierSnapshot::capture);
        let result = match master {
            Some(master) => crate::net::link::topology::set_master(
                rtnl,
                &target_netns,
                iface.clone(),
                Some(master),
            ),
            None => Err(SystemError::EINVAL),
        };
        if let Err(error) = result {
            return Err(rollback_new_link(rtnl, &target_netns, iface, error));
        }
        if let Some(bridge_carrier) = bridge_carrier {
            bridge_carrier.notify_if_changed();
        }
    }
    notify_link_change(&iface);
    if let Some((peer, _, _)) = peer {
        notify_link_change(&peer);
    }
    Ok(Vec::new())
}
