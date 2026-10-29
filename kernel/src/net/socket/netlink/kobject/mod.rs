use crate::net::socket::netlink::{common::NetlinkSocket, table::NetlinkKobjectUeventProtocol};

mod bound;
pub(super) use bound::send_userspace;
pub mod message;

pub(super) type NetlinkKobjectUeventSocket = NetlinkSocket<NetlinkKobjectUeventProtocol>;
