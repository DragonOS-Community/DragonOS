use crate::net::socket::netlink::message::attr::{Attribute, CAttrHeader};
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u16)]
#[expect(non_camel_case_types)]
#[expect(clippy::upper_case_acronyms)]
pub(crate) enum NeighAttrClass {
    UNSPEC = 0,
    DST = 1,
    LLADDR = 2,
    CACHEINFO = 3,
    PROBES = 4,
    VLAN = 5,
    PORT = 6,
    VNI = 7,
    IFINDEX = 8,
    MASTER = 9,
    LINK_NETNSID = 10,
    SRC_VNI = 11,
    PROTOCOL = 12,
    NH_ID = 13,
    FDB_EXT_ATTRS = 14,
    FLAGS_EXT = 15,
    NDM_STATE_MASK = 16,
    NDM_FLAGS_MASK = 17,
}

impl TryFrom<u16> for NeighAttrClass {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::UNSPEC),
            1 => Ok(Self::DST),
            2 => Ok(Self::LLADDR),
            3 => Ok(Self::CACHEINFO),
            4 => Ok(Self::PROBES),
            5 => Ok(Self::VLAN),
            6 => Ok(Self::PORT),
            7 => Ok(Self::VNI),
            8 => Ok(Self::IFINDEX),
            9 => Ok(Self::MASTER),
            10 => Ok(Self::LINK_NETNSID),
            11 => Ok(Self::SRC_VNI),
            12 => Ok(Self::PROTOCOL),
            13 => Ok(Self::NH_ID),
            14 => Ok(Self::FDB_EXT_ATTRS),
            15 => Ok(Self::FLAGS_EXT),
            16 => Ok(Self::NDM_STATE_MASK),
            17 => Ok(Self::NDM_FLAGS_MASK),
            _ => Err(SystemError::EINVAL),
        }
    }
}

/// A structurally decoded neighbour attribute.
///
/// Attribute policy is intentionally applied by the RTM operation handler:
/// Linux validates the complete policy for NEW/dump, while DEL searches only
/// for its first NDA_DST and ignores malformed unrelated attributes.
#[derive(Debug, Clone)]
pub struct NeighAttr {
    class: NeighAttrClass,
    nested: bool,
    payload: Vec<u8>,
}

impl NeighAttr {
    pub(crate) fn destination(payload: Vec<u8>) -> Self {
        Self {
            class: NeighAttrClass::DST,
            nested: false,
            payload,
        }
    }

    pub(crate) fn link_local_address(payload: Vec<u8>) -> Self {
        Self {
            class: NeighAttrClass::LLADDR,
            nested: false,
            payload,
        }
    }

    pub(crate) fn cache_info(payload: Vec<u8>) -> Self {
        Self {
            class: NeighAttrClass::CACHEINFO,
            nested: false,
            payload,
        }
    }

    pub(crate) fn probes(payload: Vec<u8>) -> Self {
        Self {
            class: NeighAttrClass::PROBES,
            nested: false,
            payload,
        }
    }

    pub(crate) fn class(&self) -> NeighAttrClass {
        self.class
    }

    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub(crate) fn is_nested(&self) -> bool {
        self.nested
    }
}

impl Attribute for NeighAttr {
    const ALLOW_TRAILING: bool = true;

    fn type_(&self) -> u16 {
        self.class as u16
    }

    fn payload_as_bytes(&self) -> &[u8] {
        &self.payload
    }

    fn read_from_buf(header: &CAttrHeader, payload: &[u8]) -> Result<Option<Self>, SystemError>
    where
        Self: Sized,
    {
        let Ok(class) = NeighAttrClass::try_from(header.type_()) else {
            // Netlink's non-strict parsing ignores attributes newer than the
            // receiver. They are not selectors for the supported operations.
            return Ok(None);
        };
        let mut copy = Vec::new();
        copy.try_reserve_exact(payload.len())
            .map_err(|_| SystemError::ENOMEM)?;
        copy.extend_from_slice(payload);
        Ok(Some(Self {
            class,
            nested: header.is_nested(),
            payload: copy,
        }))
    }
}
