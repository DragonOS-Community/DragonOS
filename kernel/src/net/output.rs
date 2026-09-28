//! Local-output policy over one complete, already admitted IP datagram.
//!
//! UDP and raw sockets must execute OUTPUT and the original POST_ROUTING
//! synchronously with sendto(2). The output owner retains the packet after
//! those verdicts and handles device/neighbour backpressure asynchronously.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::cell::{Cell, RefCell};

use smoltcp::wire::{IpAddress, IpVersion, Ipv4Address, Ipv4Packet, Ipv6Address, Ipv6Packet};
use system_error::SystemError;

use crate::{
    driver::net::{
        local_output::PreparedIpOutputReservation, local_queue::OutputCtContext,
        types::InterfaceFlags, Iface, LocalPacketOrigin,
    },
    process::namespace::net_namespace::NetNamespace,
};

use super::{
    conntrack::{
        parse_ipv4_conntrack_with_mode, parse_ipv6_conntrack_with_mode, CtAddress, CtChecksumMode,
        CtDirection, CtMatch, CtNatRequest, CtPacketContext, CtPacketState, NatManipSide,
    },
    nftables::{
        NftIpv4Hook, NftNatAction, NftNatEvent, NftNatPacket, NftNatProgress, NftPacket,
        RulesetSnapshot,
    },
    route::{OutputRouteDecision, RTN_BROADCAST, RTN_LOCAL, RTN_MULTICAST},
};

fn hook_oifname(
    netns: &NetNamespace,
    ruleset: &RulesetSnapshot,
    version: IpVersion,
    hook: NftIpv4Hook,
    route: OutputRouteDecision,
) -> Result<[u8; 16], SystemError> {
    let needed = match version {
        IpVersion::Ipv4 => ruleset.hook_requires_iface_names(hook),
        IpVersion::Ipv6 => ruleset.ipv6_hook_requires_iface_names(hook),
    };
    if !needed {
        return Ok([0; 16]);
    }
    let hook_oif = if route.kind == RTN_LOCAL {
        crate::net::LOOPBACK_IFINDEX
    } else {
        route.oif as usize
    };
    let device = netns
        .device_list()
        .get(&hook_oif)
        .cloned()
        .ok_or(SystemError::ENETUNREACH)?;
    let mut name = [0; 16];
    device.common().with_iface_name(|current| {
        let bytes = current.as_bytes();
        name[..bytes.len().min(15)].copy_from_slice(&bytes[..bytes.len().min(15)]);
    });
    Ok(name)
}

fn destination(bytes: &[u8], version: IpVersion) -> Result<IpAddress, SystemError> {
    match version {
        IpVersion::Ipv4 => Ipv4Packet::new_checked(bytes)
            .map(|packet| packet.dst_addr().into())
            .map_err(|_| SystemError::EINVAL),
        IpVersion::Ipv6 => Ipv6Packet::new_checked(bytes)
            .map(|packet| packet.dst_addr().into())
            .map_err(|_| SystemError::EINVAL),
    }
}

/// The first route may have been selected before OUTPUT. A destination NAT
/// action must re-run FIB with the caller's original OIF constraint, whereas
/// SNAT/MASQ in POST cannot change this decision.
fn route_after_output(
    netns: &Arc<NetNamespace>,
    ruleset: &RulesetSnapshot,
    version: IpVersion,
    initial_destination: IpAddress,
    new_destination: IpAddress,
    mut route: OutputRouteDecision,
) -> Result<(OutputRouteDecision, Option<(CtAddress, u32)>), SystemError> {
    let may_masquerade = ruleset.requires_masquerade(version, NftIpv4Hook::PostRouting);
    if initial_destination == new_destination && !may_masquerade {
        return Ok((route, None));
    }
    let router = netns.router();
    let mut routes = super::route::lock_output_routes(&router, netns.device_list());
    if initial_destination != new_destination {
        route = routes
            .lookup(new_destination, route.required_oif)
            .ok_or(SystemError::ENETUNREACH)?;
        let egress = routes
            .ingress_device(route.oif)
            .ok_or(SystemError::ENETUNREACH)?;
        if route.kind != RTN_LOCAL && !egress.flags().contains(InterfaceFlags::UP) {
            return Err(SystemError::ENETDOWN);
        }
    }
    let masquerade = if may_masquerade {
        routes.prepare_masquerade_addresses()?;
        let hook_oif = if route.kind == RTN_LOCAL {
            crate::net::LOOPBACK_IFINDEX as u32
        } else {
            route.oif
        };
        routes
            .masquerade_address(hook_oif, new_destination, route.next_hop)
            .map(|address| {
                let address = match address {
                    IpAddress::Ipv4(address) => CtAddress::V4(address.octets()),
                    IpAddress::Ipv6(address) => CtAddress::V6(address.octets()),
                };
                (address, hook_oif)
            })
    } else {
        None
    };
    Ok((route, masquerade))
}

/// The ruleset pinned for this datagram owns the decision to run CT. A
/// newly activated table must not reinterpret an older in-flight packet.
/// A fork represents one actual multicast/broadcast copy after OUTPUT.
pub(crate) struct LocalOutputCt<'a> {
    netns: &'a NetNamespace,
    ruleset: &'a RulesetSnapshot,
    version: IpVersion,
    tracked: bool,
    context: RefCell<Option<CtPacketContext>>,
    mark: Cell<u32>,
}

impl<'a> LocalOutputCt<'a> {
    pub(crate) fn new(
        netns: &'a NetNamespace,
        ruleset: &'a RulesetSnapshot,
        version: IpVersion,
    ) -> Self {
        Self {
            netns,
            ruleset,
            version,
            tracked: ruleset.conntrack_registered(version)
                && netns.conntrack().is_active_family(version),
            context: RefCell::new(None),
            mark: Cell::new(0),
        }
    }

    pub(crate) fn fork(&self) -> Self {
        Self {
            netns: self.netns,
            ruleset: self.ruleset,
            version: self.version,
            tracked: self.tracked,
            context: RefCell::new(self.context.borrow().clone()),
            mark: Cell::new(self.mark.get()),
        }
    }

    pub(crate) fn mark(&self) -> u32 {
        self.mark.get()
    }

    /// A multicast/broadcast local clone shares one conntrack identity with
    /// its original. If the clone reaches POST and confirms first, the
    /// original must reuse that same NAT binding at its own POST hook.
    fn adopt_confirmed(&self, context: &OutputCtContext) {
        if self.tracked {
            self.context.borrow_mut().replace(context.for_ingress());
        }
    }

    fn classify(&self, bytes: &[u8]) -> bool {
        if !self.tracked || self.context.borrow().is_some() {
            return true;
        }
        let parsed = match self.version {
            IpVersion::Ipv4 => parse_ipv4_conntrack_with_mode(bytes, CtChecksumMode::Skip),
            IpVersion::Ipv6 => parse_ipv6_conntrack_with_mode(bytes, CtChecksumMode::Skip),
        };
        self.context.borrow_mut().replace(
            self.netns
                .conntrack()
                .classify(parsed, crate::time::Instant::now()),
        );
        true
    }

    fn nat_event(
        &self,
        bytes: &mut [u8],
        event: NftNatEvent,
        masquerade: Option<(CtAddress, u32)>,
        applied: &mut bool,
        current_side: &mut Option<NatManipSide>,
    ) -> Result<NftNatProgress, SystemError> {
        let mut context = self.context.borrow_mut();
        let context = context.as_mut().ok_or(SystemError::EPERM)?;
        match event {
            NftNatEvent::Begin(side) => {
                *applied = false;
                *current_side = Some(side);
                match context {
                    CtPacketContext::Candidate(candidate) if !candidate.nat_initialized(side) => {
                        Ok(NftNatProgress::Continue)
                    }
                    CtPacketContext::Candidate(candidate) => {
                        let rewrite = candidate.nat_rewrite(side);
                        if rewrite.from != rewrite.to {
                            rewrite
                                .apply(bytes, CtChecksumMode::Skip)
                                .map_err(|_| SystemError::EPERM)?;
                        }
                        *applied = true;
                        Ok(NftNatProgress::SkipRules)
                    }
                    _ => {
                        context
                            .rewrite_confirmed_nat(bytes, side, CtChecksumMode::Skip)
                            .map_err(|_| SystemError::EPERM)?;
                        *applied = true;
                        Ok(NftNatProgress::SkipRules)
                    }
                }
            }
            NftNatEvent::Rule(action) => {
                let (side, request) = match action {
                    NftNatAction::Dnat(request) => (NatManipSide::Destination, request),
                    NftNatAction::Snat(request) => (NatManipSide::Source, request),
                    NftNatAction::Masquerade { ports } => {
                        let (address, ifindex) = masquerade.ok_or(SystemError::EPERM)?;
                        let request = CtNatRequest::masquerade(address, ifindex, ports)
                            .map_err(|_| SystemError::EINVAL)?;
                        (NatManipSide::Source, request)
                    }
                };
                if *current_side != Some(side) || *applied {
                    return Err(SystemError::EINVAL);
                }
                let CtPacketContext::Candidate(candidate) = context else {
                    return Err(SystemError::EPERM);
                };
                self.netns
                    .conntrack()
                    .select_nat_mapping(candidate, side, request, crate::time::Instant::now())
                    .map_err(|error| match error {
                        super::conntrack::CtError::NoMemory => SystemError::ENOMEM,
                        _ => SystemError::EPERM,
                    })?;
                Ok(NftNatProgress::SkipRules)
            }
            NftNatEvent::Finish(side) => {
                if *current_side != Some(side) {
                    return Err(SystemError::EINVAL);
                }
                *current_side = None;
                if *applied {
                    return Ok(NftNatProgress::SkipRules);
                }
                let CtPacketContext::Candidate(candidate) = context else {
                    return Err(SystemError::EPERM);
                };
                if !candidate.nat_initialized(side) {
                    candidate
                        .initialize_null_binding(side)
                        .map_err(|_| SystemError::EPERM)?;
                }
                let rewrite = candidate.nat_rewrite(side);
                if rewrite.from != rewrite.to {
                    rewrite
                        .apply(bytes, CtChecksumMode::Skip)
                        .map_err(|_| SystemError::EPERM)?;
                }
                *applied = true;
                Ok(NftNatProgress::SkipRules)
            }
        }
    }

    pub(crate) fn evaluate_ipv4(
        &self,
        hook: NftIpv4Hook,
        bytes: &mut [u8],
        oifname: [u8; 16],
        addr_type: &dyn Fn(Ipv4Address) -> u8,
        masquerade: Option<(CtAddress, u32)>,
    ) -> Result<bool, SystemError> {
        if !self.ruleset.hook_may_nat(IpVersion::Ipv4, hook) {
            return Ok(self.allows_ipv4(hook, bytes, oifname, addr_type));
        }
        let mut applied = false;
        let mut current_side = None;
        // Linux nf_nat_redirect_ipv4 sends locally generated packets to lo.
        let redirect = |_: &[u8]| Some(CtAddress::V4([127, 0, 0, 1]));
        let mut packet = NftNatPacket::new_ipv4(bytes, &self.context, [0; 16], oifname, addr_type)
            .with_mark(&self.mark);
        if hook == NftIpv4Hook::LocalOut {
            packet = packet.with_redirect_address(&redirect);
        }
        self.ruleset.evaluate_ipv4_hook_with_nat(
            hook,
            packet,
            |bytes| self.classify(bytes),
            |bytes, event| {
                self.nat_event(bytes, event, masquerade, &mut applied, &mut current_side)
            },
        )
    }

    pub(crate) fn evaluate_ipv6(
        &self,
        hook: NftIpv4Hook,
        bytes: &mut [u8],
        oifname: [u8; 16],
        local_destination: Option<&dyn Fn(Ipv6Address) -> bool>,
        masquerade: Option<(CtAddress, u32)>,
    ) -> Result<bool, SystemError> {
        if !self.ruleset.hook_may_nat(IpVersion::Ipv6, hook) {
            return Ok(self.allows_ipv6(hook, bytes, oifname, local_destination));
        }
        let mut applied = false;
        let mut current_side = None;
        let mut packet =
            NftNatPacket::new_ipv6(bytes, &self.context, [0; 16], oifname).with_mark(&self.mark);
        // Linux nf_nat_redirect_ipv6 uses ::1 at LOCAL_OUT.
        let redirect = |_: &[u8]| {
            let mut address = [0; 16];
            address[15] = 1;
            Some(CtAddress::V6(address))
        };
        if hook == NftIpv4Hook::LocalOut {
            packet = packet.with_redirect_address(&redirect);
        }
        if let Some(lookup) = local_destination {
            packet = packet.with_ipv6_local_destination(lookup);
        }
        self.ruleset.evaluate_ipv6_hook_with_nat(
            hook,
            packet,
            |bytes| self.classify(bytes),
            |bytes, event| {
                self.nat_event(bytes, event, masquerade, &mut applied, &mut current_side)
            },
        )
    }

    pub(crate) fn allows_ipv4(
        &self,
        hook: NftIpv4Hook,
        bytes: &[u8],
        oifname: [u8; 16],
        addr_type: &dyn Fn(Ipv4Address) -> u8,
    ) -> bool {
        if !self.ruleset.has_ipv4_hook(hook) {
            return hook != NftIpv4Hook::LocalOut || self.classify(bytes);
        }
        let packet = NftPacket::new(bytes, addr_type)
            .with_interface_names([0; 16], oifname)
            .with_conntrack_cell(&self.context)
            .with_mark(&self.mark);
        self.ruleset
            .allows_ipv4_hook_with_tracking(hook, &packet, || {
                hook != NftIpv4Hook::LocalOut || self.classify(bytes)
            })
    }

    pub(crate) fn allows_ipv6(
        &self,
        hook: NftIpv4Hook,
        bytes: &[u8],
        oifname: [u8; 16],
        local_destination: Option<&dyn Fn(Ipv6Address) -> bool>,
    ) -> bool {
        if !self.ruleset.has_ipv6_hook(hook) {
            return hook != NftIpv4Hook::LocalOut || self.classify(bytes);
        }
        let mut packet = NftPacket::new_ipv6(bytes)
            .with_interface_names([0; 16], oifname)
            .with_conntrack_cell(&self.context)
            .with_mark(&self.mark);
        if let Some(lookup) = local_destination {
            packet = packet.with_ipv6_local_destination(lookup);
        }
        self.ruleset
            .allows_ipv6_hook_with_tracking(hook, &packet, || {
                hook != NftIpv4Hook::LocalOut || self.classify(bytes)
            })
    }

    /// Run the fixed terminal confirmation only after the last POST chain
    /// accepts. Retain the confirmed identity for local ingress/retry.
    pub(crate) fn confirm(self) -> Result<OutputCtContext, SystemError> {
        if !self.tracked {
            return Ok(OutputCtContext::Untracked);
        }
        let context = self.context.into_inner().ok_or(SystemError::EPERM)?;
        let mut context = Box::try_new(context).map_err(|_| SystemError::ENOMEM)?;
        if let CtPacketContext::Candidate(candidate) = &*context {
            let confirmed = self
                .netns
                .confirm_conntrack(candidate.clone(), crate::time::Instant::now())
                .map_err(|_| SystemError::EPERM)?;
            let flow = match confirmed {
                crate::net::conntrack::CtConfirm::Inserted(flow)
                | crate::net::conntrack::CtConfirm::Reused(flow) => flow,
            };
            *context = CtPacketContext::Matched(CtMatch {
                flow,
                direction: CtDirection::Original,
                state: CtPacketState::New,
            });
        }
        Ok(OutputCtContext::Tracked(context))
    }
}

/// Admit a fully serialized IPv6 UDP datagram before sendto(2) returns.
/// OUTPUT and the original POST_ROUTING run on the exact bytes that the
/// bounded output owner will later transmit; its retry path does not rerun
/// either hook.
pub(crate) fn submit_prepared_ipv6(
    netns: &Arc<NetNamespace>,
    mut reservation: PreparedIpOutputReservation<'_>,
    mut route: OutputRouteDecision,
) -> Result<(), SystemError> {
    reservation.validate_for_ipv6_route(route)?;
    let initial_destination = destination(reservation.bytes(), IpVersion::Ipv6)?;
    let _egress = netns
        .device_list()
        .get(&(route.oif as usize))
        .cloned()
        .ok_or(SystemError::ENETUNREACH)?;
    let ruleset = netns.nftables().snapshot();
    let output_oifname = hook_oifname(
        netns,
        &ruleset,
        IpVersion::Ipv6,
        NftIpv4Hook::LocalOut,
        route,
    )?;
    let ct = LocalOutputCt::new(netns, &ruleset, IpVersion::Ipv6);
    let router = netns.router();
    let output_routes = ruleset
        .ipv6_hook_requires_local_destination(NftIpv4Hook::LocalOut)
        .then(|| super::route::lock_output_routes(&router, netns.device_list()));
    let output_is_local = |address: Ipv6Address| {
        output_routes.as_ref().is_some_and(|routes| {
            routes
                .lookup(IpAddress::Ipv6(address), None)
                .is_some_and(|decision| decision.kind == RTN_LOCAL)
        })
    };
    let output_lookup = output_routes
        .as_ref()
        .map(|_| &output_is_local as &dyn Fn(Ipv6Address) -> bool);
    if !ct.evaluate_ipv6(
        NftIpv4Hook::LocalOut,
        reservation.bytes_mut(),
        output_oifname,
        output_lookup,
        None,
    )? {
        return Err(SystemError::EPERM);
    }
    drop(output_routes);
    let (selected, masquerade) = route_after_output(
        netns,
        &ruleset,
        IpVersion::Ipv6,
        initial_destination,
        destination(reservation.bytes(), IpVersion::Ipv6)?,
        route,
    )?;
    route = selected;
    reservation.validate_for_ipv6_route(route)?;
    let post_oifname = hook_oifname(
        netns,
        &ruleset,
        IpVersion::Ipv6,
        NftIpv4Hook::PostRouting,
        route,
    )?;
    let post_routes = ruleset
        .ipv6_hook_requires_local_destination(NftIpv4Hook::PostRouting)
        .then(|| super::route::lock_output_routes(&router, netns.device_list()));
    let post_is_local = |address: Ipv6Address| {
        post_routes.as_ref().is_some_and(|routes| {
            routes
                .lookup(IpAddress::Ipv6(address), None)
                .is_some_and(|decision| decision.kind == RTN_LOCAL)
        })
    };
    let post_lookup = post_routes
        .as_ref()
        .map(|_| &post_is_local as &dyn Fn(Ipv6Address) -> bool);
    if !ct.evaluate_ipv6(
        NftIpv4Hook::PostRouting,
        reservation.bytes_mut(),
        post_oifname,
        post_lookup,
        masquerade,
    )? {
        return Err(SystemError::EPERM);
    }
    drop(post_routes);
    let mark = ct.mark();
    let ct_context = ct.confirm()?;
    reservation.commit_ipv6(
        route,
        netns.next_ipv6_fragment_identification(),
        ct_context,
        mark,
    )
}

/// Submit an IPv4 datagram after its complete buffer and source-owner queue
/// slot have been reserved. A local multicast/broadcast copy runs its own
/// POST_ROUTING hook before the original, as in Linux's ip_mc_output().
pub(crate) fn submit_prepared_ipv4(
    netns: &Arc<NetNamespace>,
    mut reservation: PreparedIpOutputReservation<'_>,
    mut route: OutputRouteDecision,
    multicast_loop: bool,
    may_fragment: bool,
) -> Result<Option<Arc<dyn Iface>>, SystemError> {
    reservation.validate_for_route(route, may_fragment)?;
    let packet = Ipv4Packet::new_checked(reservation.bytes()).map_err(|_| SystemError::EINVAL)?;
    let ttl = packet.hop_limit();
    let initial_destination = IpAddress::Ipv4(packet.dst_addr());
    let ruleset = netns.nftables().snapshot();
    let output_oifname = hook_oifname(
        netns,
        &ruleset,
        IpVersion::Ipv4,
        NftIpv4Hook::LocalOut,
        route,
    )?;
    let addr_type = |address| crate::net::route::ipv4_addr_type(netns, address);
    let ct = LocalOutputCt::new(netns, &ruleset, IpVersion::Ipv4);

    if !ct.evaluate_ipv4(
        NftIpv4Hook::LocalOut,
        reservation.bytes_mut(),
        output_oifname,
        &addr_type,
        None,
    )? {
        return Err(SystemError::EPERM);
    }

    let (selected, masquerade) = route_after_output(
        netns,
        &ruleset,
        IpVersion::Ipv4,
        initial_destination,
        destination(reservation.bytes(), IpVersion::Ipv4)?,
        route,
    )?;
    route = selected;
    reservation.validate_for_route(route, may_fragment)?;
    let egress = netns
        .device_list()
        .get(&(route.oif as usize))
        .cloned()
        .ok_or(SystemError::ENETUNREACH)?;
    // An IPv4 multicast/broadcast local clone is delivered through the
    // route's egress, which may differ from the socket's source owner. Pin
    // that device's namespace epoch independently: a concurrent veth move
    // must not deliver this old-namespace clone into the target namespace.
    let egress_epoch = egress.common().namespace_epoch();
    let egress_still_owned = egress_epoch & 1 == 0
        && egress
            .net_namespace()
            .is_some_and(|owner| Arc::ptr_eq(&owner, netns));
    let is_physical_egress = !egress.flags().contains(InterfaceFlags::LOOPBACK);
    let post_oifname = hook_oifname(
        netns,
        &ruleset,
        IpVersion::Ipv4,
        NftIpv4Hook::PostRouting,
        route,
    )?;

    let clone_local = egress_still_owned
        && is_physical_egress
        && ((route.kind == RTN_MULTICAST && multicast_loop) || route.kind == RTN_BROADCAST);
    let mut local_copy_queued = false;
    if clone_local {
        // A failed clone allocation/delivery must not cancel the original.
        // Copying precedes the clone's POST hook, matching skb_clone failure.
        let mut clone = Vec::new();
        if clone.try_reserve_exact(reservation.bytes().len()).is_ok() {
            clone.extend_from_slice(reservation.bytes());
            let copy_ct = ct.fork();
            if matches!(
                copy_ct.evaluate_ipv4(
                    NftIpv4Hook::PostRouting,
                    &mut clone,
                    post_oifname,
                    &addr_type,
                    masquerade,
                ),
                Ok(true)
            ) {
                let copy_mark = copy_ct.mark();
                if let Ok(context) = copy_ct.confirm() {
                    ct.adopt_confirmed(&context);
                    local_copy_queued = crate::driver::net::inject_owned_local_ip_packet_if_epoch(
                        egress.as_ref(),
                        route.oif,
                        egress.mac(),
                        clone,
                        route.kind == RTN_BROADCAST,
                        LocalPacketOrigin::LocalOutput,
                        Some(context.for_ingress()),
                        copy_mark,
                        Some(egress_epoch),
                    )
                    .is_ok();
                }
            }
        }
    }

    // Multicast TTL zero may still have a local copy, but must not pass the
    // original through POST_ROUTING or emit it to the physical device.
    if route.kind == RTN_MULTICAST && ttl == 0 && is_physical_egress {
        return Ok(local_copy_queued.then_some(egress));
    }
    if !ct.evaluate_ipv4(
        NftIpv4Hook::PostRouting,
        reservation.bytes_mut(),
        post_oifname,
        &addr_type,
        masquerade,
    )? {
        return Err(SystemError::EPERM);
    }

    let mark = ct.mark();
    reservation.commit(route, may_fragment, ct.confirm()?, mark)?;
    let local_delivery = route.kind == RTN_LOCAL || !is_physical_egress || local_copy_queued;
    Ok(local_delivery.then_some(egress))
}
