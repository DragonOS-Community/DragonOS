use crate::net::socket::{
    netlink::{
        message::segment::{common::SegmentCommon, SegmentBody},
        route::message::attr::neigh::NeighAttr,
    },
    AddressFamily,
};
use system_error::SystemError;

use super::route::RouteType;

pub type NeighSegment = SegmentCommon<NeighSegmentBody, NeighAttr>;

impl SegmentBody for NeighSegmentBody {
    type CType = CNdMsg;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CNdMsg {
    pub family: u8,
    pub pad1: u8,
    pub pad2: u16,
    pub ifindex: i32,
    pub state: u16,
    pub flags: u8,
    pub type_: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct NeighSegmentBody {
    pub family: AddressFamily,
    pub ifindex: i32,
    pub state: NeighState,
    pub flags: u8,
    pub kind: RouteType,
}

impl TryFrom<CNdMsg> for NeighSegmentBody {
    type Error = SystemError;

    fn try_from(value: CNdMsg) -> Result<Self, Self::Error> {
        Ok(Self {
            family: AddressFamily::try_from(value.family as u16)?,
            ifindex: value.ifindex,
            state: NeighState::from_bits_truncate(value.state),
            flags: value.flags,
            kind: RouteType::try_from(value.type_)?,
        })
    }
}

impl From<NeighSegmentBody> for CNdMsg {
    fn from(value: NeighSegmentBody) -> Self {
        Self {
            family: value.family as u8,
            pad1: 0,
            pad2: 0,
            ifindex: value.ifindex,
            state: value.state.bits(),
            flags: value.flags,
            type_: value.kind as u8,
        }
    }
}

bitflags::bitflags! {
    pub struct NeighState: u16 {
        const INCOMPLETE = 0x01;
        const REACHABLE = 0x02;
        const STALE = 0x04;
        const DELAY = 0x08;
        const PROBE = 0x10;
        const FAILED = 0x20;
        const NOARP = 0x40;
        const PERMANENT = 0x80;
    }
}
