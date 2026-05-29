use crate::net::socket::netlink::message::attr::{Attribute, CAttrHeader};
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug, Clone, Copy)]
#[repr(u16)]
enum NeighAttrClass {
    UNSPEC = 0,
    DST = 1,
    LLADDR = 2,
}

impl TryFrom<u16> for NeighAttrClass {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::UNSPEC),
            1 => Ok(Self::DST),
            2 => Ok(Self::LLADDR),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[derive(Debug, Clone)]
pub enum NeighAttr {
    Destination(Vec<u8>),
    LinkLocalAddress(Vec<u8>),
}

impl NeighAttr {
    fn class(&self) -> NeighAttrClass {
        match self {
            Self::Destination(_) => NeighAttrClass::DST,
            Self::LinkLocalAddress(_) => NeighAttrClass::LLADDR,
        }
    }
}

impl Attribute for NeighAttr {
    fn type_(&self) -> u16 {
        self.class() as u16
    }

    fn payload_as_bytes(&self) -> &[u8] {
        match self {
            Self::Destination(bytes) | Self::LinkLocalAddress(bytes) => bytes.as_slice(),
        }
    }

    fn read_from_buf(header: &CAttrHeader, payload_buf: &[u8]) -> Result<Option<Self>, SystemError>
    where
        Self: Sized,
    {
        let Ok(class) = NeighAttrClass::try_from(header.type_()) else {
            return Ok(None);
        };

        let attr = match class {
            NeighAttrClass::DST => Self::Destination(payload_buf.to_vec()),
            NeighAttrClass::LLADDR => Self::LinkLocalAddress(payload_buf.to_vec()),
            NeighAttrClass::UNSPEC => return Ok(None),
        };

        Ok(Some(attr))
    }
}
