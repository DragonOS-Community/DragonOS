use crate::net::socket::{endpoint::Endpoint, netlink::addr::multicast::GroupIdSet};
use system_error::SystemError;

pub mod multicast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetlinkSocketAddr {
    port: u32,
    groups: GroupIdSet,
}

impl NetlinkSocketAddr {
    pub fn new(port_num: u32, groups: GroupIdSet) -> Self {
        Self {
            port: port_num,
            groups,
        }
    }

    pub fn new_unspecified() -> Self {
        Self {
            port: 0,
            groups: GroupIdSet::new_empty(),
        }
    }

    pub const fn port(&self) -> u32 {
        self.port
    }

    pub fn groups(&self) -> GroupIdSet {
        self.groups
    }

    pub fn add_groups(&mut self, groups: GroupIdSet) {
        self.groups.add_groups(groups);
    }
}

impl TryFrom<Endpoint> for NetlinkSocketAddr {
    type Error = SystemError;

    fn try_from(value: Endpoint) -> Result<Self, Self::Error> {
        match value {
            Endpoint::Netlink(addr) => Ok(addr),
            _ => Err(SystemError::EAFNOSUPPORT),
        }
    }
}

impl From<NetlinkSocketAddr> for Endpoint {
    fn from(value: NetlinkSocketAddr) -> Self {
        Endpoint::Netlink(value)
    }
}
