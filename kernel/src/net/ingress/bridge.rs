//! IP netfilter for packets that entered through a bridge port.

use alloc::sync::Arc;
use core::cell::{Cell, RefCell};

use super::{
    hooks::{HookDeviceNames, IngressHooks},
    IngressStage,
};
use crate::{
    driver::net::{Iface, PacketBuffer},
    net::{
        conntrack::CtPacketContext,
        nftables::{NftIpv4Hook, RulesetSnapshot},
        route::lock_output_routes,
    },
    process::namespace::net_namespace::NetNamespace,
};
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpAddress, IpVersion, Ipv4Packet, Ipv6Packet,
};
use system_error::SystemError;

pub(crate) const ETHERNET_HEADER_LEN: usize = 14;

/// Shared policy for one received frame. Bridge topology and frame ownership
/// remain with the caller. The bridge name is the only nonempty name seen
/// by a pure bridge IP hook.
pub(crate) struct BridgeIpHooks<'a> {
    netns: &'a NetNamespace,
    ruleset: Arc<RulesetSnapshot>,
    version: IpVersion,
    ifindex: u32,
    names: HookDeviceNames<'a>,
}

impl<'a> BridgeIpHooks<'a> {
    pub(crate) fn new(netns: &'a NetNamespace, bridge: &dyn Iface, version: IpVersion) -> Self {
        let ifindex = bridge.nic_id() as u32;
        let mut name = [0; 16];
        bridge.common().with_iface_name(|current| {
            let len = current.len().min(15);
            name[..len].copy_from_slice(&current.as_bytes()[..len]);
        });
        Self {
            netns,
            ruleset: netns.nftables().snapshot(),
            version,
            ifindex,
            names: HookDeviceNames::Bridge(ifindex, name),
        }
    }

    pub(crate) fn has_hooks(&self) -> bool {
        [
            NftIpv4Hook::PreRouting,
            NftIpv4Hook::Forward,
            NftIpv4Hook::PostRouting,
        ]
        .into_iter()
        .any(|hook| match self.version {
            IpVersion::Ipv4 => self.ruleset.has_ipv4_hook(hook),
            IpVersion::Ipv6 => self.ruleset.has_ipv6_hook(hook),
        })
    }

    pub(crate) fn needs_conntrack(&self) -> bool {
        self.ruleset.conntrack_registered(self.version)
            && self.netns.conntrack().is_active_family(self.version)
    }

    pub(crate) fn pre_routing(
        &self,
        frame: &mut IpFrame<'_>,
        context: &BridgePacketContext,
    ) -> bool {
        self.run(NftIpv4Hook::PreRouting, frame, context, self.ifindex, 0)
    }

    pub(crate) fn forward(&self, frame: &mut IpFrame<'_>, context: &BridgePacketContext) -> bool {
        self.run(
            NftIpv4Hook::Forward,
            frame,
            context,
            self.ifindex,
            self.ifindex,
        ) && self.run(NftIpv4Hook::PostRouting, frame, context, 0, self.ifindex)
    }

    pub(crate) fn confirm(&self, context: &BridgePacketContext) -> Option<CtPacketContext> {
        self.executor(context, None)
            .confirm_accepted_packet_context(self.version)
    }

    fn run(
        &self,
        hook: NftIpv4Hook,
        frame: &mut IpFrame<'_>,
        context: &BridgePacketContext,
        iif: u32,
        oif: u32,
    ) -> bool {
        let may_nat = self.ruleset.hook_may_nat(self.version, hook);
        let needs_lookup = match self.version {
            IpVersion::Ipv4 => self.ruleset.ipv4_hook_requires_route_lookup(hook),
            IpVersion::Ipv6 => self.ruleset.ipv6_hook_requires_route_lookup(hook),
        };
        let masquerade = self.ruleset.requires_masquerade(self.version, hook);
        let needs_addresses = masquerade || self.ruleset.requires_redirect(self.version, hook);
        let router = self.netns.router();
        let mut routes = (needs_lookup || needs_addresses)
            .then(|| lock_output_routes(&router, self.netns.device_list()));
        if needs_addresses
            && routes.as_mut().is_some_and(|routes| {
                if masquerade && self.version == IpVersion::Ipv4 {
                    routes.prepare_masquerade_addresses().is_err()
                } else {
                    routes
                        .prepare_device_addresses(if iif != 0 { iif } else { oif })
                        .is_err()
                }
            })
        {
            return false;
        }
        let executor = self.executor(context, routes.as_ref());
        match (self.version, may_nat) {
            (IpVersion::Ipv4, false) => executor.allows_ipv4(hook, frame.ip(), iif, oif),
            (IpVersion::Ipv6, false) => executor.allows_ipv6(hook, frame.ip(), iif, oif),
            (IpVersion::Ipv4, true) => {
                let next_hop = (oif != 0).then(|| frame.destination(self.version));
                frame
                    .writable_ip()
                    .is_ok_and(|ip| executor.allows_ipv4_nat(hook, ip, iif, oif, next_hop))
            }
            (IpVersion::Ipv6, true) => {
                let next_hop = (oif != 0).then(|| frame.destination(self.version));
                frame
                    .writable_ip()
                    .is_ok_and(|ip| executor.allows_ipv6_nat(hook, ip, iif, oif, next_hop))
            }
        }
    }

    fn executor<'b>(
        &'b self,
        context: &'b BridgePacketContext,
        routes: Option<&'b crate::net::route::OutputRouteGuard<'b>>,
    ) -> IngressHooks<'b> {
        IngressHooks {
            ruleset: &self.ruleset,
            netns: self.netns,
            device_names: self.names,
            stage: IngressStage::Pending,
            mark: &context.mark,
            packet_context: &context.conntrack,
            fib_routes: routes,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct BridgePacketContext {
    pub(crate) mark: Cell<u32>,
    pub(crate) conntrack: RefCell<Option<CtPacketContext>>,
}

impl BridgePacketContext {
    pub(crate) fn fork(&self) -> Self {
        Self {
            mark: Cell::new(self.mark.get()),
            conntrack: RefCell::new(self.conntrack.borrow().clone()),
        }
    }
}

/// The borrowed frame stays cheap for stateless filtering. NAT makes only
/// its own branch writable, so later flood copies start with PRE's bytes.
pub(crate) struct IpFrame<'a> {
    original: &'a [u8],
    modified: Option<PacketBuffer>,
}

impl<'a> IpFrame<'a> {
    pub(crate) fn checked(frame: &'a [u8], version: IpVersion) -> Option<Self> {
        let ethernet = EthernetFrame::new_checked(frame).ok()?;
        let payload = ethernet.payload();
        let ip_len = match (version, ethernet.ethertype()) {
            (IpVersion::Ipv4, EthernetProtocol::Ipv4) => {
                let packet = Ipv4Packet::new_checked(payload).ok()?;
                if packet.version() != 4 || !packet.verify_checksum() {
                    return None;
                }
                usize::from(packet.total_len())
            }
            (IpVersion::Ipv6, EthernetProtocol::Ipv6) => {
                let packet = Ipv6Packet::new_checked(payload).ok()?;
                if packet.version() != 6 {
                    return None;
                }
                if packet.next_header() == smoltcp::wire::IpProtocol::HopByHop
                    && smoltcp::wire::Ipv6ExtHeader::new_checked(packet.payload()).is_err()
                {
                    return None;
                }
                packet.total_len()
            }
            _ => return None,
        };
        Some(Self {
            original: &frame[..ETHERNET_HEADER_LEN + ip_len],
            modified: None,
        })
    }

    pub(crate) fn fork(&self) -> IpFrame<'_> {
        IpFrame {
            original: self.bytes(),
            modified: None,
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.modified.as_deref().unwrap_or(self.original)
    }

    pub(crate) fn ip(&self) -> &[u8] {
        &self.bytes()[ETHERNET_HEADER_LEN..]
    }

    fn writable_frame(&mut self) -> Result<&mut [u8], SystemError> {
        if self.modified.is_none() {
            self.modified = Some(PacketBuffer::try_from_slice(self.original)?);
        }
        Ok(self.modified.as_deref_mut().unwrap())
    }

    fn writable_ip(&mut self) -> Result<&mut [u8], SystemError> {
        Ok(&mut self.writable_frame()?[ETHERNET_HEADER_LEN..])
    }

    pub(crate) fn set_destination_mac(
        &mut self,
        destination: smoltcp::wire::EthernetAddress,
    ) -> Result<(), SystemError> {
        self.writable_frame()?[..6].copy_from_slice(destination.as_bytes());
        Ok(())
    }

    pub(crate) fn destination(&self, version: IpVersion) -> IpAddress {
        match version {
            IpVersion::Ipv4 => Ipv4Packet::new_unchecked(self.ip()).dst_addr().into(),
            IpVersion::Ipv6 => Ipv6Packet::new_unchecked(self.ip()).dst_addr().into(),
        }
    }
}
