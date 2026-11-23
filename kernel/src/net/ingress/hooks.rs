//! IP hook execution over one packet's policy context, independent of the
//! ingress route decision and of the interface receive queue.

use super::IngressStage;
use crate::{
    net::{
        conntrack::{
            parse_ipv4_conntrack, parse_ipv6_conntrack, CtAddress, CtChecksumMode, CtConfirm,
            CtDirection, CtMatch, CtNatRequest, CtPacketContext, CtPacketState, NatManipSide,
        },
        nftables::{
            NftDeviceNames, NftIpv4Hook, NftIpv6RouteLookup, NftNatAction, NftNatEvent,
            NftNatPacket, NftNatProgress, NftPacket, RulesetSnapshot,
        },
        route::{OutputRouteGuard, RTN_UNICAST, RTN_UNREACHABLE},
    },
    process::namespace::net_namespace::NetNamespace,
};
use core::cell::{Cell, RefCell};
use smoltcp::wire::{IpAddress, IpVersion, Ipv4Packet, Ipv6Packet};
use system_error::SystemError;

pub(super) struct IngressHooks<'a> {
    pub(super) ruleset: &'a RulesetSnapshot,
    pub(super) netns: &'a NetNamespace,
    pub(super) device_names: &'a NftDeviceNames,
    pub(super) stage: IngressStage,
    pub(super) mark: &'a Cell<u32>,
    pub(super) packet_context: &'a RefCell<Option<CtPacketContext>>,
    pub(super) fib_routes: Option<&'a OutputRouteGuard<'a>>,
}

impl IngressHooks<'_> {
    pub(super) fn ct_active_for(&self, version: IpVersion) -> bool {
        // Runtime resources may be activated immediately before publishing a
        // new ruleset. In-flight packets pinned to the old snapshot must
        // retain that snapshot's untracked semantics during the handover.
        self.ruleset.conntrack_registered(version)
            && self.netns.conntrack().is_active_family(version)
    }

    /// Classify at the fixed -200 step in the compiled hook program. Local
    /// reinjection retains OUTPUT's context instead of creating a second one.
    fn classify_ingress(&self, version: IpVersion, bytes: &[u8]) -> bool {
        if !self.ct_active_for(version) {
            return true;
        }
        if self.stage == IngressStage::LocalOutput {
            return self.packet_context.borrow().is_some();
        }
        let parsed = match version {
            IpVersion::Ipv4 => parse_ipv4_conntrack(bytes),
            IpVersion::Ipv6 => parse_ipv6_conntrack(bytes),
        };
        let context = self
            .netns
            .conntrack()
            .classify(parsed, crate::time::Instant::now());
        self.packet_context.borrow_mut().replace(context);
        true
    }

    /// A packet rejected by a later filter never publishes its private flow.
    /// Only INPUT or POSTROUTING may confirm a new candidate. Return the
    /// confirmed identity so a deferred forwarding error stays RELATED.
    pub(super) fn confirm_accepted_packet_context(
        &self,
        version: IpVersion,
    ) -> Option<CtPacketContext> {
        if !self.ct_active_for(version) {
            return Some(CtPacketContext::Untracked);
        }
        let context = self.packet_context.borrow_mut().take()?;
        match context {
            CtPacketContext::Candidate(candidate) => {
                let confirmed = self
                    .netns
                    .confirm_conntrack(candidate, crate::time::Instant::now())
                    .ok()?;
                let flow = match confirmed {
                    CtConfirm::Inserted(flow) | CtConfirm::Reused(flow) => flow,
                };
                Some(CtPacketContext::Matched(CtMatch {
                    flow,
                    direction: CtDirection::Original,
                    state: CtPacketState::New,
                }))
            }
            other => Some(other),
        }
    }

    pub(super) fn confirm_accepted_packet(&self, version: IpVersion) -> bool {
        self.confirm_accepted_packet_context(version).is_some()
    }

    /// The fixed outer NAT event runs at -100/+100 inside the ruleset's
    /// priority-ordered hook program. The selected first-packet mapping stays
    /// private until INPUT/POSTROUTING confirmation. Packet bytes change only
    /// at Finish, so a failed rule never leaves a partially translated skb.
    fn nat_event(
        &self,
        bytes: &mut [u8],
        event: NftNatEvent,
        oif: u32,
        next_hop: Option<IpAddress>,
        active_side: &Cell<Option<NatManipSide>>,
    ) -> Result<NftNatProgress, SystemError> {
        let mode = if self.stage == IngressStage::LocalOutput {
            CtChecksumMode::Skip
        } else {
            CtChecksumMode::Verify
        };
        match event {
            NftNatEvent::Begin(side) => {
                let context = self.packet_context.borrow();
                let context = context.as_ref().ok_or(SystemError::EINVAL)?;
                // The loopback receive copy has already traversed OUTPUT and
                // POSTROUTING with this same CT identity. PRE/IN filter chains
                // still run, but neither direction may bind or rewrite again.
                if self.stage == IngressStage::LocalOutput {
                    return Ok(NftNatProgress::SkipRules);
                }
                match context {
                    CtPacketContext::Candidate(candidate) if !candidate.nat_initialized(side) => {
                        active_side.set(Some(side));
                        Ok(NftNatProgress::Continue)
                    }
                    CtPacketContext::Candidate(candidate) => {
                        candidate
                            .nat_rewrite(side)
                            .apply(bytes, mode)
                            .map_err(|_| SystemError::EINVAL)?;
                        Ok(NftNatProgress::SkipRules)
                    }
                    _ => {
                        context
                            .rewrite_confirmed_nat(bytes, side, mode)
                            .map_err(|_| SystemError::EINVAL)?;
                        Ok(NftNatProgress::SkipRules)
                    }
                }
            }
            NftNatEvent::Rule(action) => {
                let expected_side = active_side.get().ok_or(SystemError::EINVAL)?;
                let mut context = self.packet_context.borrow_mut();
                let Some(CtPacketContext::Candidate(candidate)) = context.as_mut() else {
                    return Err(SystemError::EINVAL);
                };
                let (side, request) = match action {
                    NftNatAction::Dnat(request) => (NatManipSide::Destination, request),
                    NftNatAction::Snat(request) => (NatManipSide::Source, request),
                    NftNatAction::Masquerade { ports } => {
                        let destination = match IpVersion::of_packet(bytes) {
                            Ok(IpVersion::Ipv4) => Ipv4Packet::new_checked(&*bytes)
                                .map(|packet| IpAddress::Ipv4(packet.dst_addr())),
                            Ok(IpVersion::Ipv6) => Ipv6Packet::new_checked(&*bytes)
                                .map(|packet| IpAddress::Ipv6(packet.dst_addr())),
                            _ => return Err(SystemError::EINVAL),
                        }
                        .map_err(|_| SystemError::EINVAL)?;
                        let next_hop = next_hop.ok_or(SystemError::EINVAL)?;
                        let address = self
                            .fib_routes
                            .and_then(|routes| {
                                routes.masquerade_address(oif, destination, next_hop)
                            })
                            .ok_or(SystemError::EINVAL)?;
                        let address = match address {
                            IpAddress::Ipv4(address) => CtAddress::V4(address.octets()),
                            IpAddress::Ipv6(address) => CtAddress::V6(address.octets()),
                        };
                        let request = CtNatRequest::masquerade(address, oif, ports)
                            .map_err(|_| SystemError::EINVAL)?;
                        (NatManipSide::Source, request)
                    }
                };
                if side != expected_side {
                    return Err(SystemError::EINVAL);
                }
                self.netns
                    .conntrack()
                    .select_nat_mapping(candidate, side, request, crate::time::Instant::now())
                    .map_err(|_| SystemError::EINVAL)?;
                Ok(NftNatProgress::SkipRules)
            }
            NftNatEvent::Finish(side) => {
                match active_side.replace(None) {
                    None => return Ok(NftNatProgress::Continue),
                    Some(active) if active == side => {}
                    Some(_) => return Err(SystemError::EINVAL),
                }
                let mut context = self.packet_context.borrow_mut();
                if let Some(CtPacketContext::Candidate(candidate)) = context.as_mut() {
                    if !candidate.nat_initialized(side) {
                        candidate
                            .initialize_null_binding(side)
                            .map_err(|_| SystemError::EINVAL)?;
                    }
                    candidate
                        .nat_rewrite(side)
                        .apply(bytes, mode)
                        .map_err(|_| SystemError::EINVAL)?;
                }
                Ok(NftNatProgress::Continue)
            }
        }
    }

    pub(super) fn allows_ipv4(&self, hook: NftIpv4Hook, bytes: &[u8], iif: u32, oif: u32) -> bool {
        if !(self.ruleset.has_ipv4_hook(hook)
            || self.ct_active_for(IpVersion::Ipv4) && hook == NftIpv4Hook::PreRouting)
        {
            return true;
        }
        if self.ruleset.ipv4_hook_requires_route_lookup(hook) && self.fib_routes.is_none() {
            // A route-aware rule must never run with an unprotected FIB view.
            return false;
        }
        let lookup = |address| {
            self.fib_routes
                .map_or(RTN_UNICAST, |routes| routes.ipv4_addr_type(address))
        };
        let (iifname, oifname) = if self.ruleset.hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let packet = NftPacket::new(bytes, &lookup)
            .with_interface_names(iifname, oifname)
            .with_conntrack_cell(self.packet_context)
            .with_mark(self.mark);
        self.ruleset
            .allows_ipv4_hook_with_tracking(hook, &packet, || {
                hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv4, bytes)
            })
    }

    pub(super) fn allows_ipv6(&self, hook: NftIpv4Hook, bytes: &[u8], iif: u32, oif: u32) -> bool {
        if !(self.ruleset.has_ipv6_hook(hook)
            || self.ct_active_for(IpVersion::Ipv6) && hook == NftIpv4Hook::PreRouting)
        {
            return true;
        }
        if self.ruleset.ipv6_hook_requires_route_lookup(hook) && self.fib_routes.is_none() {
            return false;
        }
        let resolve = |address, device| {
            self.fib_routes.map_or(RTN_UNREACHABLE, |routes| {
                routes.ipv6_addr_type(address, device)
            })
        };
        let lookup = NftIpv6RouteLookup {
            resolve: &resolve,
            device: Some(if hook == NftIpv4Hook::PostRouting {
                oif
            } else {
                iif
            })
            .filter(|index| *index != 0),
        };
        let (iifname, oifname) = if self.ruleset.ipv6_hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let packet = NftPacket::new_ipv6(bytes)
            .with_ipv6_route_lookup(lookup)
            .with_interface_names(iifname, oifname)
            .with_conntrack_cell(self.packet_context)
            .with_mark(self.mark);
        self.ruleset
            .allows_ipv6_hook_with_tracking(hook, &packet, || {
                hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv6, bytes)
            })
    }

    pub(super) fn allows_ipv4_nat(
        &self,
        hook: NftIpv4Hook,
        bytes: &mut [u8],
        iif: u32,
        oif: u32,
        next_hop: Option<IpAddress>,
    ) -> bool {
        if self.ruleset.ipv4_hook_requires_route_lookup(hook) && self.fib_routes.is_none() {
            return false;
        }
        let lookup = |address| {
            self.fib_routes
                .map_or(RTN_UNICAST, |routes| routes.ipv4_addr_type(address))
        };
        let (iifname, oifname) = if self.ruleset.hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let active_side = Cell::new(None);
        let redirect = |bytes: &[u8]| {
            let destination = Ipv4Packet::new_checked(bytes).ok()?.dst_addr();
            let IpAddress::Ipv4(address) = self
                .fib_routes?
                .redirect_address(iif, IpAddress::Ipv4(destination))?
            else {
                return None;
            };
            Some(CtAddress::V4(address.octets()))
        };
        self.ruleset
            .evaluate_ipv4_hook_with_nat(
                hook,
                NftNatPacket::new_ipv4(bytes, self.packet_context, iifname, oifname, &lookup)
                    .with_mark(self.mark)
                    .with_redirect_address(&redirect),
                |bytes| {
                    hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv4, bytes)
                },
                |bytes, event| self.nat_event(bytes, event, oif, next_hop, &active_side),
            )
            .unwrap_or(false)
    }

    pub(super) fn allows_ipv6_nat(
        &self,
        hook: NftIpv4Hook,
        bytes: &mut [u8],
        iif: u32,
        oif: u32,
        next_hop: Option<IpAddress>,
    ) -> bool {
        if self.ruleset.ipv6_hook_requires_route_lookup(hook) && self.fib_routes.is_none() {
            return false;
        }
        let resolve = |address, device| {
            self.fib_routes.map_or(RTN_UNREACHABLE, |routes| {
                routes.ipv6_addr_type(address, device)
            })
        };
        let lookup = NftIpv6RouteLookup {
            resolve: &resolve,
            device: Some(if hook == NftIpv4Hook::PostRouting {
                oif
            } else {
                iif
            })
            .filter(|index| *index != 0),
        };
        let (iifname, oifname) = if self.ruleset.ipv6_hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let active_side = Cell::new(None);
        let redirect = |bytes: &[u8]| {
            let destination = Ipv6Packet::new_checked(bytes).ok()?.dst_addr();
            let IpAddress::Ipv6(address) = self
                .fib_routes?
                .redirect_address(iif, IpAddress::Ipv6(destination))?
            else {
                return None;
            };
            Some(CtAddress::V6(address.octets()))
        };
        self.ruleset
            .evaluate_ipv6_hook_with_nat(
                hook,
                NftNatPacket::new_ipv6(bytes, self.packet_context, iifname, oifname)
                    .with_ipv6_route_lookup(lookup)
                    .with_mark(self.mark)
                    .with_redirect_address(&redirect),
                |bytes| {
                    hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv6, bytes)
                },
                |bytes, event| self.nat_event(bytes, event, oif, next_hop, &active_side),
            )
            .unwrap_or(false)
    }
}
