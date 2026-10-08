//! IPv6 multicast loopback is a clone of the already POST-routed wire packet.
//! Each source fragment gets the clone's POST hook, as in ip6_finish_output2.

use super::*;
use crate::driver::net::local_output::{ipv6_fragment, ipv6_fragment_boundary};

fn post_clone(ct: &LocalOutputCt<'_>, bytes: &mut [u8], oifname: [u8; 16]) -> bool {
    let router = ct.netns.router();
    let routes = ct
        .ruleset
        .ipv6_hook_requires_local_destination(NftIpv4Hook::PostRouting)
        .then(|| super::super::route::lock_output_routes(&router, ct.netns.device_list()));
    let is_local = |address: Ipv6Address| {
        routes.as_ref().is_some_and(|routes| {
            routes
                .lookup(IpAddress::Ipv6(address), None)
                .is_some_and(|route| route.kind == RTN_LOCAL)
        })
    };
    let lookup = routes
        .as_ref()
        .map(|_| &is_local as &dyn Fn(Ipv6Address) -> bool);
    if !ct
        .ruleset
        .hook_may_nat(IpVersion::Ipv6, NftIpv4Hook::PostRouting)
    {
        return ct.allows_ipv6(NftIpv4Hook::PostRouting, bytes, oifname, lookup);
    }
    let mut packet =
        NftNatPacket::new_ipv6(bytes, &ct.context, [0; 16], oifname).with_mark(&ct.mark);
    if let Some(lookup) = lookup {
        packet = packet.with_ipv6_local_destination(lookup);
    }
    // Linux retains IPS_SRC_NAT_DONE on the clone. The original has already
    // rewritten these bytes; running its source rewrite again would reject
    // the translated tuple. Filtering/counters still execute for every clone.
    ct.ruleset
        .evaluate_ipv6_hook_with_nat(
            NftIpv4Hook::PostRouting,
            packet,
            |bytes| ct.classify(bytes),
            |_bytes, event| match event {
                NftNatEvent::Begin(_) | NftNatEvent::Finish(_) => Ok(NftNatProgress::SkipRules),
                NftNatEvent::Rule(_) => Err(SystemError::EINVAL),
            },
        )
        .unwrap_or(false)
}

fn deliver_clone(
    ct: &LocalOutputCt<'_>,
    egress: &Arc<dyn Iface>,
    epoch: u64,
    route: OutputRouteDecision,
    mut frame: Vec<u8>,
    oifname: [u8; 16],
) {
    let clone_ct = ct.fork();
    if !post_clone(&clone_ct, &mut frame, oifname) {
        return;
    }
    let mark = clone_ct.mark();
    let Ok(context) = clone_ct.confirm() else {
        return;
    };
    // No FIB, policy or conntrack lock spans device admission. A failed clone
    // does not cancel the original or other fragments' loopback attempts.
    let _ = crate::driver::net::inject_owned_local_ip_packet_if_epoch(
        egress.as_ref(),
        route.oif,
        egress.mac(),
        frame,
        false,
        LocalPacketOrigin::LocalOutput,
        Some(context.for_ingress()),
        mark,
        Some(epoch),
    );
}

pub(super) fn ipv6_loopback(
    ct: &LocalOutputCt<'_>,
    egress: &Arc<dyn Iface>,
    epoch: u64,
    route: OutputRouteDecision,
    frame: &[u8],
    identification: u32,
    oifname: [u8; 16],
) {
    if frame.len() <= route.ip_mtu {
        let mut clone = Vec::new();
        if clone.try_reserve_exact(frame.len()).is_ok() {
            clone.extend_from_slice(frame);
            deliver_clone(ct, egress, epoch, route, clone, oifname);
        }
        return;
    }
    let Ok((split, _)) = ipv6_fragment_boundary(frame) else {
        return;
    };
    if route.ip_mtu < split + 16 {
        return;
    }
    let available = route.ip_mtu - split - 8;
    let payload_len = frame.len() - split;
    let mut offset = 0;
    while offset < payload_len {
        match ipv6_fragment(frame, route.ip_mtu, offset, identification) {
            Ok((fragment, chunk)) => {
                offset += chunk;
                deliver_clone(ct, egress, epoch, route, fragment, oifname);
            }
            Err(SystemError::ENOMEM) => {
                // A failed skb_clone on one fragment must not suppress the
                // later wire fragments. Advance exactly the shared builder's
                // fragment extent even when it cannot allocate this clone.
                let remaining = payload_len - offset;
                let chunk = if remaining > available {
                    available & !7
                } else {
                    remaining
                };
                if chunk == 0 {
                    return;
                }
                offset += chunk;
            }
            Err(_) => return,
        }
    }
}
