use crate::net::socket::netlink::{common::NetlinkSocket, table::NetlinkRouteProtocol};

pub mod bound;
mod kernel;
pub mod message;

pub type NetlinkRouteSocket = NetlinkSocket<NetlinkRouteProtocol>;
