use core::result::Result;
use heapless::{LinearMap, Vec};

#[cfg(any(feature = "proto-ipv4", feature = "proto-ipv6"))]
use super::{check, DispatchError, IpPayload, Packet};
use super::{Interface, InterfaceInner};
use crate::config::{IFACE_MAX_ADDR_COUNT, IFACE_MAX_MULTICAST_GROUP_COUNT};
use crate::phy::{Device, PacketMeta};
use crate::wire::*;

const MULTICAST_RETRY_DELAY: crate::time::Duration = crate::time::Duration::from_millis(200);

#[cfg(all(test, feature = "proto-ipv6", feature = "medium-ethernet"))]
mod ownership_tests {
    use super::*;
    use crate::phy::Medium;

    fn address(tail: u16) -> Ipv6Address {
        Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, tail)
    }

    #[test]
    fn explicit_unrelated_solicited_group_survives_address_update() {
        let (mut iface, _, _) = crate::tests::setup(Medium::Ethernet);
        let group = address(0x1234).solicited_node();
        iface.join_multicast_group(group).unwrap();
        assert!(iface.has_multicast_group(group));
        iface.update_ip_addrs(|addresses| {
            addresses.clear();
            addresses
                .push(IpCidr::new(address(0x5678).into(), 64))
                .unwrap();
        });
        assert!(iface.has_multicast_group(group));
        iface.leave_multicast_group(group).unwrap();
        assert!(!iface.has_multicast_group(group));
    }

    #[test]
    fn explicit_owner_survives_last_corresponding_address_removal() {
        let (mut iface, _, _) = crate::tests::setup(Medium::Ethernet);
        let local = address(0x1234);
        let group = local.solicited_node();
        iface.update_ip_addrs(|addresses| {
            addresses.clear();
            addresses.push(IpCidr::new(local.into(), 64)).unwrap();
        });
        iface.join_multicast_group(group).unwrap();
        iface.update_ip_addrs(|addresses| addresses.clear());
        assert!(iface.has_multicast_group(group));
        iface.leave_multicast_group(group).unwrap();
        assert!(!iface.has_multicast_group(group));
    }

    #[test]
    fn explicit_leave_keeps_address_owner_and_rejoin_is_idempotent() {
        let (mut iface, _, _) = crate::tests::setup(Medium::Ethernet);
        let local = address(0x1234);
        let group = local.solicited_node();
        iface.update_ip_addrs(|addresses| {
            addresses.clear();
            addresses.push(IpCidr::new(local.into(), 64)).unwrap();
        });
        iface.join_multicast_group(group).unwrap();
        iface.join_multicast_group(group).unwrap();
        iface.leave_multicast_group(group).unwrap();
        assert!(iface.has_multicast_group(group));
        iface.leave_multicast_group(group).unwrap();
        assert!(iface.has_multicast_group(group));
        iface.update_ip_addrs(|addresses| addresses.clear());
        assert!(!iface.has_multicast_group(group));
    }

    #[test]
    fn shared_solicited_node_remains_until_last_address_disappears() {
        let (mut iface, _, _) = crate::tests::setup(Medium::Ethernet);
        let first = address(0x1234);
        let second = Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x1234);
        let group = first.solicited_node();
        assert_eq!(group, second.solicited_node());
        iface.update_ip_addrs(|addresses| {
            addresses.clear();
            addresses.push(IpCidr::new(first.into(), 64)).unwrap();
            addresses.push(IpCidr::new(second.into(), 64)).unwrap();
        });
        iface.update_ip_addrs(|addresses| {
            addresses.remove(0);
        });
        assert!(iface.has_multicast_group(group));
        iface.update_ip_addrs(|addresses| addresses.clear());
        assert!(!iface.has_multicast_group(group));
    }
}

/// Error type for `join_multicast_group`, `leave_multicast_group`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum MulticastError {
    /// The table of joined multicast groups is already full.
    GroupTableFull,
    /// Cannot join/leave the given multicast group.
    Unaddressable,
}

#[cfg(feature = "proto-ipv4")]
pub enum IgmpReportState {
    Inactive,
    ToGeneralQuery {
        version: IgmpVersion,
        timeout: crate::time::Instant,
        interval: crate::time::Duration,
        next_index: usize,
    },
    ToSpecificQuery {
        version: IgmpVersion,
        timeout: crate::time::Instant,
        group: Ipv4Address,
    },
}

#[cfg(feature = "proto-ipv6")]
pub(crate) enum MldReportState {
    Inactive,
    ToGeneralQuery {
        timeout: crate::time::Instant,
    },
    ToSpecificQuery {
        group: Ipv6Address,
        timeout: crate::time::Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupState {
    /// Joining group, we have to send the join packet.
    Joining,
    /// We've already sent the join packet, we have nothing to do.
    Joined,
    /// We want to leave the group, we have to send a leave packet.
    Leaving,
}

const EXPLICIT_OWNER: u8 = 1;
#[cfg(feature = "proto-ipv6")]
const SOLICITED_NODE_OWNER: u8 = 2;

/// Report progress and subscription ownership are independent. In particular,
/// changing local addresses must not revoke an application's explicit join.
#[derive(Debug, Clone, Copy)]
struct Group {
    state: GroupState,
    owners: u8,
}

pub struct State {
    groups: LinearMap<IpAddress, Group, IFACE_MAX_MULTICAST_GROUP_COUNT>,
    /// Back off only after a transient transmit failure. A permanent policy
    /// or route denial still finishes this report attempt.
    retry_at: Option<crate::time::Instant>,
    /// When to report for (all or) the next multicast group membership via IGMP
    #[cfg(feature = "proto-ipv4")]
    igmp_report_state: IgmpReportState,
    #[cfg(feature = "proto-ipv6")]
    mld_report_state: MldReportState,
}

impl State {
    pub fn new() -> Self {
        Self {
            groups: LinearMap::new(),
            retry_at: None,
            #[cfg(feature = "proto-ipv4")]
            igmp_report_state: IgmpReportState::Inactive,
            #[cfg(feature = "proto-ipv6")]
            mld_report_state: MldReportState::Inactive,
        }
    }

    pub(super) fn retry_at(&self) -> Option<crate::time::Instant> {
        self.retry_at
    }

    pub fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        // Return false if we don't have the multicast group,
        // or we're leaving it.
        self.groups
            .get(&addr.into())
            .is_some_and(|group| group.owners != 0)
    }
}

impl core::fmt::Display for MulticastError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            MulticastError::GroupTableFull => write!(f, "GroupTableFull"),
            MulticastError::Unaddressable => write!(f, "Unaddressable"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for MulticastError {}

impl Interface {
    /// Add an address to a list of subscribed multicast IP addresses.
    pub fn join_multicast_group<T: Into<IpAddress>>(
        &mut self,
        addr: T,
    ) -> Result<(), MulticastError> {
        self.join_group(addr.into(), EXPLICIT_OWNER)
    }

    fn join_group(&mut self, addr: IpAddress, owner: u8) -> Result<(), MulticastError> {
        if !addr.is_multicast() {
            return Err(MulticastError::Unaddressable);
        }

        if let Some(group) = self.inner.multicast.groups.get_mut(&addr) {
            group.owners |= owner;
            if group.state == GroupState::Leaving {
                group.state = GroupState::Joined;
            }
        } else {
            self.inner
                .multicast
                .groups
                .insert(
                    addr,
                    Group {
                        state: GroupState::Joining,
                        owners: owner,
                    },
                )
                .map_err(|_| MulticastError::GroupTableFull)?;
        }
        Ok(())
    }

    /// Remove an address from the subscribed multicast IP addresses.
    pub fn leave_multicast_group<T: Into<IpAddress>>(
        &mut self,
        addr: T,
    ) -> Result<(), MulticastError> {
        self.leave_group(addr.into(), EXPLICIT_OWNER)
    }

    fn leave_group(&mut self, addr: IpAddress, owner: u8) -> Result<(), MulticastError> {
        if !addr.is_multicast() {
            return Err(MulticastError::Unaddressable);
        }

        if let Some(group) = self.inner.multicast.groups.get_mut(&addr) {
            group.owners &= !owner;
            if group.owners != 0 {
                return Ok(());
            }
            let delete;
            (group.state, delete) = match group.state {
                GroupState::Joining => (GroupState::Joined, true),
                GroupState::Joined => (GroupState::Leaving, false),
                GroupState::Leaving => (GroupState::Leaving, false),
            };
            if delete {
                self.inner.multicast.groups.remove(&addr);
            }
        }
        Ok(())
    }

    /// Check whether the interface listens to given destination multicast IP address.
    pub fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        self.inner.has_multicast_group(addr)
    }

    #[cfg(feature = "proto-ipv6")]
    pub(super) fn update_solicited_node_groups(&mut self) {
        // Remove old solicited-node multicast addresses
        let removals: Vec<_, IFACE_MAX_MULTICAST_GROUP_COUNT> = self
            .inner
            .multicast
            .groups
            .iter()
            .filter(|(a, group)| {
                group.owners & SOLICITED_NODE_OWNER != 0
                    && matches!(a, IpAddress::Ipv6(a) if !self.inner.has_solicited_node(*a))
            })
            .map(|(a, _)| *a)
            .collect();
        for removal in removals {
            let _ = self.leave_group(removal, SOLICITED_NODE_OWNER);
        }

        let cidrs: Vec<IpCidr, IFACE_MAX_ADDR_COUNT> = Vec::from_slice(self.ip_addrs()).unwrap();
        for cidr in cidrs {
            if let IpCidr::Ipv6(cidr) = cidr {
                let _ =
                    self.join_group(cidr.address().solicited_node().into(), SOLICITED_NODE_OWNER);
            }
        }
    }

    /// Do multicast egress.
    ///
    /// - Send join/leave packets according to the multicast group state.
    /// - Depending on `igmp_report_state` and the therein contained
    ///   timeouts, send IGMP membership reports.
    ///
    /// A policy or permanent route denial drops this one report, as LOCAL_OUT
    /// does on Linux. Capacity and neighbor discovery are transient: retain
    /// the group/query state so the same report can be retried.
    fn dispatch_multicast_report(
        &mut self,
        device: &mut (impl Device + ?Sized),
        packet: Packet<'_>,
    ) -> bool {
        if self
            .inner
            .multicast
            .retry_at
            .is_some_and(|retry_at| self.inner.now < retry_at)
        {
            return false;
        }
        if !device.policy_current() {
            return false;
        }
        let Some(tx_token) = device.transmit(self.inner.now) else {
            self.inner.multicast.retry_at = Some(self.inner.now + MULTICAST_RETRY_DELAY);
            return false;
        };
        match self.inner.dispatch_ip(
            tx_token,
            PacketMeta::default(),
            packet,
            &mut self.fragmenter,
        ) {
            Ok(())
            | Err(
                DispatchError::PolicyDrop | DispatchError::NoRoute | DispatchError::MtuRetry(_),
            ) => {
                self.inner.multicast.retry_at = None;
                true
            }
            Err(DispatchError::Exhausted | DispatchError::NeighborPending) => {
                self.inner.multicast.retry_at = Some(self.inner.now + MULTICAST_RETRY_DELAY);
                false
            }
        }
    }

    pub fn multicast_egress(&mut self, device: &mut (impl Device + ?Sized)) {
        if self
            .inner
            .multicast
            .retry_at
            .is_some_and(|retry_at| self.inner.now < retry_at)
        {
            return;
        }
        self.inner.multicast.retry_at = None;
        // Process multicast joins.
        while let Some((&addr, _)) = self
            .inner
            .multicast
            .groups
            .iter()
            .find(|(_, group)| group.state == GroupState::Joining)
        {
            match addr {
                #[cfg(feature = "proto-ipv4")]
                IpAddress::Ipv4(addr) => {
                    if let Some(pkt) = self.inner.igmp_report_packet(IgmpVersion::Version2, addr) {
                        if !self.dispatch_multicast_report(device, pkt) {
                            break;
                        }
                    }
                }
                #[cfg(feature = "proto-ipv6")]
                IpAddress::Ipv6(addr) => {
                    if let Some(pkt) = self.inner.mldv2_report_packet(&[MldAddressRecordRepr::new(
                        MldRecordType::ChangeToExclude,
                        addr,
                    )]) {
                        if !self.dispatch_multicast_report(device, pkt) {
                            break;
                        }
                    }
                }
            }

            // The report does not acquire or release any subscription owner.
            self.inner.multicast.groups.get_mut(&addr).unwrap().state = GroupState::Joined;
        }

        // Process multicast leaves.
        while let Some((&addr, _)) = self
            .inner
            .multicast
            .groups
            .iter()
            .find(|(_, group)| group.state == GroupState::Leaving)
        {
            match addr {
                #[cfg(feature = "proto-ipv4")]
                IpAddress::Ipv4(addr) => {
                    if let Some(pkt) = self.inner.igmp_leave_packet(addr) {
                        if !self.dispatch_multicast_report(device, pkt) {
                            break;
                        }
                    }
                }
                #[cfg(feature = "proto-ipv6")]
                IpAddress::Ipv6(addr) => {
                    if let Some(pkt) = self.inner.mldv2_report_packet(&[MldAddressRecordRepr::new(
                        MldRecordType::ChangeToInclude,
                        addr,
                    )]) {
                        if !self.dispatch_multicast_report(device, pkt) {
                            break;
                        }
                    }
                }
            }

            self.inner.multicast.groups.remove(&addr);
        }

        #[cfg(feature = "proto-ipv4")]
        match self.inner.multicast.igmp_report_state {
            IgmpReportState::ToSpecificQuery {
                version,
                timeout,
                group,
            } if self.inner.now >= timeout => {
                if let Some(pkt) = self.inner.igmp_report_packet(version, group) {
                    // Send initial membership report
                    if self.dispatch_multicast_report(device, pkt) {
                        self.inner.multicast.igmp_report_state = IgmpReportState::Inactive;
                    }
                }
            }
            IgmpReportState::ToGeneralQuery {
                version,
                timeout,
                interval,
                next_index,
            } if self.inner.now >= timeout => {
                let addr = self
                    .inner
                    .multicast
                    .groups
                    .iter()
                    .filter_map(|(addr, _)| match addr {
                        IpAddress::Ipv4(addr) => Some(*addr),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                    .nth(next_index);

                match addr {
                    Some(addr) => {
                        if let Some(pkt) = self.inner.igmp_report_packet(version, addr) {
                            // Send initial membership report
                            if self.dispatch_multicast_report(device, pkt) {
                                let next_timeout = (timeout + interval).max(self.inner.now);
                                self.inner.multicast.igmp_report_state =
                                    IgmpReportState::ToGeneralQuery {
                                        version,
                                        timeout: next_timeout,
                                        interval,
                                        next_index: next_index + 1,
                                    };
                            }
                        }
                    }
                    None => {
                        self.inner.multicast.igmp_report_state = IgmpReportState::Inactive;
                    }
                }
            }
            _ => {}
        }
        #[cfg(feature = "proto-ipv6")]
        match self.inner.multicast.mld_report_state {
            MldReportState::ToGeneralQuery { timeout } if self.inner.now >= timeout => {
                let records = self
                    .inner
                    .multicast
                    .groups
                    .iter()
                    .filter_map(|(addr, _)| match addr {
                        IpAddress::Ipv6(addr) => Some(MldAddressRecordRepr::new(
                            MldRecordType::ModeIsExclude,
                            *addr,
                        )),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                    .collect::<heapless::Vec<_, IFACE_MAX_MULTICAST_GROUP_COUNT>>();
                if let Some(pkt) = self.inner.mldv2_report_packet(&records) {
                    if self.dispatch_multicast_report(device, pkt) {
                        self.inner.multicast.mld_report_state = MldReportState::Inactive;
                    }
                } else {
                    self.inner.multicast.mld_report_state = MldReportState::Inactive;
                };
            }
            MldReportState::ToSpecificQuery { group, timeout } if self.inner.now >= timeout => {
                let record = MldAddressRecordRepr::new(MldRecordType::ModeIsExclude, group);
                if let Some(pkt) = self.inner.mldv2_report_packet(&[record]) {
                    if self.dispatch_multicast_report(device, pkt) {
                        self.inner.multicast.mld_report_state = MldReportState::Inactive;
                    }
                } else {
                    self.inner.multicast.mld_report_state = MldReportState::Inactive;
                }
            }
            _ => {}
        }
    }
}

impl InterfaceInner {
    /// Host duties of the **IGMPv2** protocol.
    ///
    /// Sets up `igmp_report_state` for responding to IGMP general/specific membership queries.
    /// Membership must not be reported immediately in order to avoid flooding the network
    /// after a query is broadcasted by a router; this is not currently done.
    #[cfg(feature = "proto-ipv4")]
    pub(super) fn process_igmp<'frame>(
        &mut self,
        ipv4_repr: Ipv4Repr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        use crate::time::Duration;

        let igmp_packet = check!(IgmpPacket::new_checked(ip_payload));
        let igmp_repr = check!(IgmpRepr::parse(&igmp_packet));

        // FIXME: report membership after a delay
        match igmp_repr {
            IgmpRepr::MembershipQuery {
                group_addr,
                version,
                max_resp_time,
            } => {
                // General query
                if group_addr.is_unspecified() && ipv4_repr.dst_addr == IPV4_MULTICAST_ALL_SYSTEMS {
                    let ipv4_multicast_group_count = self
                        .multicast
                        .groups
                        .keys()
                        .filter(|a| matches!(a, IpAddress::Ipv4(_)))
                        .count();

                    // Are we member in any groups?
                    if ipv4_multicast_group_count != 0 {
                        let interval = match version {
                            IgmpVersion::Version1 => Duration::from_millis(100),
                            IgmpVersion::Version2 => {
                                // No dependence on a random generator
                                // (see [#24](https://github.com/m-labs/smoltcp/issues/24))
                                // but at least spread reports evenly across max_resp_time.
                                let intervals = ipv4_multicast_group_count as u32 + 1;
                                max_resp_time / intervals
                            }
                        };
                        self.multicast.igmp_report_state = IgmpReportState::ToGeneralQuery {
                            version,
                            timeout: self.now + interval,
                            interval,
                            next_index: 0,
                        };
                    }
                } else {
                    // Group-specific query
                    if self.has_multicast_group(group_addr) && ipv4_repr.dst_addr == group_addr {
                        // Don't respond immediately
                        let timeout = max_resp_time / 4;
                        self.multicast.igmp_report_state = IgmpReportState::ToSpecificQuery {
                            version,
                            timeout: self.now + timeout,
                            group: group_addr,
                        };
                    }
                }
            }
            // Ignore membership reports
            IgmpRepr::MembershipReport { .. } => (),
            // Ignore hosts leaving groups
            IgmpRepr::LeaveGroup { .. } => (),
        }

        None
    }

    #[cfg(feature = "proto-ipv4")]
    fn igmp_report_packet<'any>(
        &self,
        version: IgmpVersion,
        group_addr: Ipv4Address,
    ) -> Option<Packet<'any>> {
        let iface_addr = self.ipv4_addr()?;
        let igmp_repr = IgmpRepr::MembershipReport {
            group_addr,
            version,
        };
        let pkt = Packet::new_ipv4(
            Ipv4Repr {
                src_addr: iface_addr,
                // Send to the group being reported
                dst_addr: group_addr,
                next_header: IpProtocol::Igmp,
                payload_len: igmp_repr.buffer_len(),
                hop_limit: 1,
                // [#183](https://github.com/m-labs/smoltcp/issues/183).
            },
            IpPayload::Igmp(igmp_repr),
        );
        Some(pkt)
    }

    #[cfg(feature = "proto-ipv4")]
    fn igmp_leave_packet<'any>(&self, group_addr: Ipv4Address) -> Option<Packet<'any>> {
        self.ipv4_addr().map(|iface_addr| {
            let igmp_repr = IgmpRepr::LeaveGroup { group_addr };
            Packet::new_ipv4(
                Ipv4Repr {
                    src_addr: iface_addr,
                    dst_addr: IPV4_MULTICAST_ALL_ROUTERS,
                    next_header: IpProtocol::Igmp,
                    payload_len: igmp_repr.buffer_len(),
                    hop_limit: 1,
                },
                IpPayload::Igmp(igmp_repr),
            )
        })
    }

    /// Host duties of the **MLDv2** protocol.
    ///
    /// Sets up `mld_report_state` for responding to MLD general/specific membership queries.
    /// Membership must not be reported immediately in order to avoid flooding the network
    /// after a query is broadcasted by a router; Currently the delay is fixed and not randomized.
    #[cfg(feature = "proto-ipv6")]
    pub(super) fn process_mldv2<'frame>(
        &mut self,
        ip_repr: Ipv6Repr,
        repr: MldRepr<'frame>,
    ) -> Option<Packet<'frame>> {
        match repr {
            MldRepr::Query {
                mcast_addr,
                max_resp_code,
                ..
            } => {
                // Do not respont immediately to the query, but wait a random time
                let delay = crate::time::Duration::from_millis(
                    (self.rand.rand_u16() % max_resp_code).into(),
                );
                // General query
                if mcast_addr.is_unspecified()
                    && (ip_repr.dst_addr == IPV6_LINK_LOCAL_ALL_NODES
                        || self.has_ip_addr(ip_repr.dst_addr))
                {
                    let ipv6_multicast_group_count = self
                        .multicast
                        .groups
                        .keys()
                        .filter(|a| matches!(a, IpAddress::Ipv6(_)))
                        .count();
                    if ipv6_multicast_group_count != 0 {
                        self.multicast.mld_report_state = MldReportState::ToGeneralQuery {
                            timeout: self.now + delay,
                        };
                    }
                }
                if self.has_multicast_group(mcast_addr) && ip_repr.dst_addr == mcast_addr {
                    self.multicast.mld_report_state = MldReportState::ToSpecificQuery {
                        group: mcast_addr,
                        timeout: self.now + delay,
                    };
                }
                None
            }
            MldRepr::Report { .. } => None,
            MldRepr::ReportRecordReprs { .. } => None,
        }
    }
}

#[cfg(all(test, feature = "medium-ip"))]
mod policy_tests {
    use super::*;
    use crate::phy::{DeviceCapabilities, IpOutputClass, IpOutputError, Medium, TxToken};
    use crate::time::Instant;

    struct RejectOutput {
        capabilities: DeviceCapabilities,
        error: Option<IpOutputError>,
        current: bool,
        stale_after_transmit: bool,
        no_token: bool,
        sent: core::cell::Cell<usize>,
    }

    struct RejectToken<'a> {
        error: Option<IpOutputError>,
        sent: &'a core::cell::Cell<usize>,
    }

    impl TxToken for RejectToken<'_> {
        fn deferred_ip_output(&self, _: IpVersion) -> bool {
            true
        }

        fn consume_full_ip<F>(
            self,
            len: usize,
            _: PacketMeta,
            _: IpOutputClass,
            _: Option<u16>,
            emit: F,
        ) -> Result<(), IpOutputError>
        where
            F: FnOnce(&mut [u8]),
        {
            if let Some(error) = self.error {
                return Err(error);
            }
            let mut bytes = alloc::vec![0; len];
            emit(&mut bytes);
            self.sent.set(self.sent.get() + 1);
            Ok(())
        }

        fn consume<R, F>(self, _: usize, _: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            panic!("multicast output must use complete IP admission")
        }
    }

    impl Device for RejectOutput {
        type RxToken<'a> = crate::tests::RxToken;
        type TxToken<'a> = RejectToken<'a>;

        fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            None
        }

        fn transmit(&mut self, _: Instant) -> Option<Self::TxToken<'_>> {
            if self.no_token {
                return None;
            }
            if self.stale_after_transmit {
                self.current = false;
            }
            Some(RejectToken {
                error: self.error,
                sent: &self.sent,
            })
        }

        fn capabilities(&self) -> DeviceCapabilities {
            self.capabilities.clone()
        }

        fn policy_current(&self) -> bool {
            self.current
        }
    }

    #[test]
    #[cfg(feature = "proto-ipv4")]
    fn igmp_policy_drop_does_not_panic_or_leave_join_pending() {
        let (mut iface, mut sockets, device) = crate::tests::setup(Medium::Ip);
        let group = Ipv4Address::new(224, 0, 0, 56);
        iface.join_multicast_group(group).unwrap();
        let mut rejecting = RejectOutput {
            capabilities: device.capabilities(),
            error: Some(IpOutputError::PolicyDrop),
            current: true,
            stale_after_transmit: false,
            no_token: false,
            sent: core::cell::Cell::new(0),
        };

        iface.poll_egress(Instant::ZERO, &mut rejecting, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(group))
                .map(|g| &g.state),
            Some(&GroupState::Joined)
        );
    }

    #[test]
    #[cfg(feature = "proto-ipv4")]
    fn igmp_capacity_failure_retains_join_for_retry() {
        let (mut iface, mut sockets, device) = crate::tests::setup(Medium::Ip);
        let group = Ipv4Address::new(224, 0, 0, 56);
        iface.join_multicast_group(group).unwrap();
        let mut rejecting = RejectOutput {
            capabilities: device.capabilities(),
            error: Some(IpOutputError::Exhausted),
            current: true,
            stale_after_transmit: false,
            no_token: false,
            sent: core::cell::Cell::new(0),
        };

        iface.poll_egress(Instant::ZERO, &mut rejecting, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(group))
                .map(|g| &g.state),
            Some(&GroupState::Joining)
        );
        assert_eq!(
            iface.poll_at(Instant::ZERO, &sockets),
            Some(Instant::from_millis(200))
        );
        iface.poll_egress(Instant::from_millis(100), &mut rejecting, &mut sockets);
        assert_eq!(
            iface.poll_at(Instant::from_millis(100), &sockets),
            Some(Instant::from_millis(200))
        );
        iface.poll_egress(Instant::from_millis(200), &mut rejecting, &mut sockets);
        assert_eq!(
            iface.poll_at(Instant::from_millis(200), &sockets),
            Some(Instant::from_millis(400))
        );
        rejecting.error = Some(IpOutputError::NoRoute);
        iface.poll_egress(Instant::from_millis(400), &mut rejecting, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(group))
                .map(|g| &g.state),
            Some(&GroupState::Joined)
        );
        assert_eq!(iface.poll_at(Instant::from_millis(400), &sockets), None);
    }

    #[test]
    #[cfg(feature = "proto-ipv4")]
    fn igmp_missing_tx_token_retries_after_capacity_recovers() {
        let (mut iface, mut sockets, device) = crate::tests::setup(Medium::Ip);
        let group = Ipv4Address::new(224, 0, 0, 58);
        iface.join_multicast_group(group).unwrap();
        let mut output = RejectOutput {
            capabilities: device.capabilities(),
            error: None,
            current: true,
            stale_after_transmit: false,
            no_token: true,
            sent: core::cell::Cell::new(0),
        };

        iface.poll_egress(Instant::ZERO, &mut output, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(group))
                .map(|g| &g.state),
            Some(&GroupState::Joining)
        );
        assert_eq!(
            iface.poll_at(Instant::ZERO, &sockets),
            Some(Instant::from_millis(200))
        );
        output.no_token = false;
        iface.poll_egress(Instant::from_millis(100), &mut output, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(group))
                .map(|g| &g.state),
            Some(&GroupState::Joining)
        );
        iface.poll_egress(Instant::from_millis(200), &mut output, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(group))
                .map(|g| &g.state),
            Some(&GroupState::Joined)
        );
        assert_eq!(output.sent.get(), 1);
        assert_eq!(iface.poll_at(Instant::from_millis(200), &sockets), None);
    }

    #[test]
    #[cfg(feature = "proto-ipv6")]
    fn mld_policy_drop_and_capacity_retry_do_not_panic() {
        let (mut iface, mut sockets, mut device) = crate::tests::setup(Medium::Ip);
        iface.poll_egress(Instant::ZERO, &mut device, &mut sockets);
        let group = Ipv6Address::new(0xff05, 0, 0, 0, 0, 0, 0, 0x00fb);
        iface.join_multicast_group(group).unwrap();
        let mut rejecting = RejectOutput {
            capabilities: device.capabilities(),
            error: Some(IpOutputError::Exhausted),
            current: true,
            stale_after_transmit: false,
            no_token: false,
            sent: core::cell::Cell::new(0),
        };

        iface.poll_egress(Instant::ZERO, &mut rejecting, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv6(group))
                .map(|g| &g.state),
            Some(&GroupState::Joining)
        );
        assert_eq!(
            iface.poll_at(Instant::ZERO, &sockets),
            Some(Instant::from_millis(200))
        );
        rejecting.error = Some(IpOutputError::PolicyDrop);
        iface.poll_egress(Instant::from_millis(200), &mut rejecting, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv6(group))
                .map(|g| &g.state),
            Some(&GroupState::Joined)
        );
    }

    #[test]
    #[cfg(feature = "proto-ipv4")]
    fn policy_change_between_multicast_reports_leaves_next_report_pending() {
        let (mut iface, mut sockets, device) = crate::tests::setup(Medium::Ip);
        let first = Ipv4Address::new(224, 0, 0, 56);
        let second = Ipv4Address::new(224, 0, 0, 57);
        iface.join_multicast_group(first).unwrap();
        iface.join_multicast_group(second).unwrap();
        let mut rejecting = RejectOutput {
            capabilities: device.capabilities(),
            error: Some(IpOutputError::PolicyDrop),
            current: true,
            stale_after_transmit: true,
            no_token: false,
            sent: core::cell::Cell::new(0),
        };

        iface.poll_egress(Instant::ZERO, &mut rejecting, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(first))
                .map(|g| &g.state),
            Some(&GroupState::Joined)
        );
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(second))
                .map(|g| &g.state),
            Some(&GroupState::Joining)
        );
        rejecting.current = true;
        rejecting.stale_after_transmit = false;
        iface.poll_egress(Instant::ZERO, &mut rejecting, &mut sockets);
        assert_eq!(
            iface
                .inner
                .multicast
                .groups
                .get(&IpAddress::Ipv4(second))
                .map(|g| &g.state),
            Some(&GroupState::Joined)
        );
    }
}
