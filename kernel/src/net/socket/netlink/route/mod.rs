use crate::net::socket::netlink::{common::NetlinkSocket, table::NetlinkRouteProtocol};

pub(super) mod bound;
pub(super) mod kernel;
pub(super) mod message;

pub(super) type NetlinkRouteSocket = NetlinkSocket<NetlinkRouteProtocol>;
