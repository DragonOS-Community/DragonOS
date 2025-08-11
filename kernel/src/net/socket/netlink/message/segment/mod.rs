use system_error::SystemError;

use crate::net::socket::netlink::message::{segment::header::CMsgSegHdr, NLMSG_ALIGN};

pub mod common;
pub mod header;

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CSegmentType {
    // Standard netlink message types
    NOOP = 1,
    ERROR = 2,
    DONE = 3,
    OVERRUN = 4,

    // protocol-level types
    NEWLINK = 16,
    DELLINK = 17,
    GETLINK = 18,
    SETLINK = 19,

    NEWADDR = 20,
    DELADDR = 21,
    GETADDR = 22,

    NEWROUTE = 24,
    DELROUTE = 25,
    GETROUTE = 26,
    // TODO 补充
}

impl TryFrom<u16> for CSegmentType {
    type Error = SystemError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(CSegmentType::NOOP),
            2 => Ok(CSegmentType::ERROR),
            3 => Ok(CSegmentType::DONE),
            4 => Ok(CSegmentType::OVERRUN),
            16 => Ok(CSegmentType::NEWLINK),
            17 => Ok(CSegmentType::DELLINK),
            18 => Ok(CSegmentType::GETLINK),
            19 => Ok(CSegmentType::SETLINK),
            20 => Ok(CSegmentType::NEWADDR),
            21 => Ok(CSegmentType::DELADDR),
            22 => Ok(CSegmentType::GETADDR),
            24 => Ok(CSegmentType::NEWROUTE),
            25 => Ok(CSegmentType::DELROUTE),
            26 => Ok(CSegmentType::GETROUTE),
            _ => Err(SystemError::EINVAL),
        }
    }
}

pub trait SegmentBody: Sized + Clone + Copy {
    type CType;

    fn read_from_buf(header: &CMsgSegHdr, buf: &[u8]) -> Result<(Self, usize), SystemError>
    where
        Self: Sized,
    {
        todo!()
    }

    fn write_to_buf(&self, buf: &mut [u8]) -> Result<(), SystemError> {
        todo!()
    }

    fn padding_len() -> usize {
        let payload_len = size_of::<Self::CType>();
        payload_len.checked_add(NLMSG_ALIGN - 1).unwrap() & (!(NLMSG_ALIGN - 1) - payload_len)
    }
}
