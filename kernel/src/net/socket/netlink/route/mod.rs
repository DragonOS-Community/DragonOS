use alloc::sync::Arc;

use crate::{
    driver::net::Iface,
    net::{
        address::AddressMutationOutcome,
        socket::netlink::{common::NetlinkSocket, table::NetlinkRouteProtocol},
    },
    process::namespace::net_namespace::NetNamespace,
};

pub(super) mod bound;
pub(super) mod kern;
pub(super) mod message;

pub(super) type NetlinkRouteSocket = NetlinkSocket<NetlinkRouteProtocol>;

/// Emits the narrow address multicast contract shared by rtnetlink and
/// in-kernel address producers such as DHCP.
pub(crate) fn notify_address_outcome(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    outcome: AddressMutationOutcome,
) {
    kern::addr::notify_address_outcome(netns, iface, outcome);
}
