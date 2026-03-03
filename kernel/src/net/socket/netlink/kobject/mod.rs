use crate::net::socket::netlink::{common::NetlinkSocket, table::NetlinkKobjectUeventProtocol};

mod bound;
pub mod message;

pub(super) type NetlinkKobjectUeventSocket = NetlinkSocket<NetlinkKobjectUeventProtocol>;
