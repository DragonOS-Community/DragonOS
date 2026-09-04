use crate::net::socket::netlink::{
    message::segment::{common::SegmentCommon, SegmentBody},
    route::message::attr::neigh::NeighAttr,
};
use system_error::SystemError;

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
    /// Raw `AF_*` value supplied by userspace.
    ///
    /// Family support is operation-specific (for example, an unsupported
    /// dump family yields an empty dump while a mutation fails).  Keeping the
    /// wire value intact lets the rtnetlink operation layer apply that policy.
    pub family: u8,
    pub ifindex: i32,
    /// Raw `NUD_*` bit set.  Validation belongs to the operation layer; using
    /// `from_bits_truncate` here would silently turn an invalid request into a
    /// different valid request.
    pub state: u16,
    pub flags: u8,
    /// Raw `RTN_*` value, for the same reason as `family` and `state`.
    pub kind: u8,
}

impl TryFrom<CNdMsg> for NeighSegmentBody {
    type Error = SystemError;

    fn try_from(value: CNdMsg) -> Result<Self, Self::Error> {
        Ok(Self {
            family: value.family,
            ifindex: value.ifindex,
            state: value.state,
            flags: value.flags,
            kind: value.type_,
        })
    }
}

impl From<NeighSegmentBody> for CNdMsg {
    fn from(value: NeighSegmentBody) -> Self {
        Self {
            family: value.family,
            pad1: 0,
            pad2: 0,
            ifindex: value.ifindex,
            state: value.state,
            flags: value.flags,
            type_: value.kind,
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
