use crate::net::socket::netlink::message::{
    attr::{Attribute, CAttrHeader},
    segment::{common::SegmentCommon, SegmentBody},
};
use system_error::SystemError;

pub type PrefixSegment = SegmentCommon<PrefixSegmentBody, PrefixAttr>;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct PrefixSegmentBody {
    pub family: u8,
    pub pad1: u8,
    pub pad2: u16,
    pub ifindex: i32,
    pub type_: u8,
    pub prefix_len: u8,
    pub flags: u8,
    pub pad3: u8,
}

impl SegmentBody for PrefixSegmentBody {
    type CType = Self;
}

#[derive(Clone, Debug)]
pub enum PrefixAttr {
    Address([u8; 16]),
    CacheInfo([u8; 8]),
}

impl PrefixAttr {
    pub fn cache_info(preferred: u32, valid: u32) -> Self {
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&preferred.to_ne_bytes());
        bytes[4..].copy_from_slice(&valid.to_ne_bytes());
        Self::CacheInfo(bytes)
    }
}

impl Attribute for PrefixAttr {
    fn type_(&self) -> u16 {
        match self {
            Self::Address(_) => 1,   // PREFIX_ADDRESS
            Self::CacheInfo(_) => 2, // PREFIX_CACHEINFO
        }
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match self {
            Self::Address(address) => address,
            Self::CacheInfo(lifetimes) => lifetimes,
        }
    }

    fn read_from_buf(_: &CAttrHeader, _: &[u8]) -> Result<Option<Self>, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
}
