use crate::net::socket::netlink::message::{
    attr::{Attribute, CAttrHeader},
    segment::{common::SegmentCommon, SegmentBody},
};
use system_error::SystemError;

pub type RuleSegment = SegmentCommon<RuleSegmentBody, RuleAttr>;

/// Linux fib_rule_hdr. In particular, the two reserved bytes are not the
/// protocol and scope fields of the similarly sized rtmsg.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleSegmentBody {
    pub family: u8,
    pub dst_len: u8,
    pub src_len: u8,
    pub tos: u8,
    pub table: u8,
    pub reserved1: u8,
    pub reserved2: u8,
    pub action: u8,
    pub flags: u32,
}

impl SegmentBody for RuleSegmentBody {
    type CType = Self;
}

const FRA_SRC: u16 = 2;
const FRA_PRIORITY: u16 = 6;
const FRA_SUPPRESS_PREFIXLEN: u16 = 14;
const FRA_TABLE: u16 = 15;
const FRA_PROTOCOL: u16 = 21;

#[derive(Debug, Clone, Copy)]
pub enum RuleAttr {
    Source([u8; 16]),
    Priority([u8; 4]),
    Table([u8; 4]),
    SuppressPrefixLength([u8; 4]),
    Protocol(u8),
}

impl Attribute for RuleAttr {
    fn type_(&self) -> u16 {
        match self {
            Self::Source(_) => FRA_SRC,
            Self::Priority(_) => FRA_PRIORITY,
            Self::Table(_) => FRA_TABLE,
            Self::SuppressPrefixLength(_) => FRA_SUPPRESS_PREFIXLEN,
            Self::Protocol(_) => FRA_PROTOCOL,
        }
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match self {
            Self::Source(bytes) => bytes,
            Self::Priority(bytes) | Self::Table(bytes) | Self::SuppressPrefixLength(bytes) => bytes,
            Self::Protocol(value) => core::slice::from_ref(value),
        }
    }

    fn read_from_buf(header: &CAttrHeader, payload: &[u8]) -> Result<Option<Self>, SystemError> {
        let invalid = |_| SystemError::EINVAL;
        let attr = match header.type_() {
            FRA_SRC => Self::Source(payload.try_into().map_err(invalid)?),
            FRA_PRIORITY => Self::Priority(payload.try_into().map_err(invalid)?),
            FRA_TABLE => Self::Table(payload.try_into().map_err(invalid)?),
            FRA_SUPPRESS_PREFIXLEN => {
                Self::SuppressPrefixLength(payload.try_into().map_err(invalid)?)
            }
            FRA_PROTOCOL => {
                let [protocol] = payload else {
                    return Err(SystemError::EINVAL);
                };
                Self::Protocol(*protocol)
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        Ok(Some(attr))
    }
}
