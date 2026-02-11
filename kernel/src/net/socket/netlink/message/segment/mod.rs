use crate::libs::align::align_up;
use crate::net::socket::netlink::message::{segment::header::CMsgSegHdr, NLMSG_ALIGN};
use alloc::fmt::Debug;
use alloc::vec::Vec;
use system_error::SystemError;

pub mod ack;
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
    type CType: Copy + TryInto<Self> + From<Self> + Debug;

    fn read_from_buf(header: &CMsgSegHdr, buf: &[u8]) -> Result<(Self, usize, usize), SystemError>
    where
        Self: Sized,
    {
        // log::info!("header: {:?}", header);
        let total_len = (header.len as usize)
            .checked_sub(size_of::<CMsgSegHdr>())
            .ok_or(SystemError::EINVAL)?;

        if buf.len() < total_len {
            return Err(SystemError::EINVAL);
        }

        let ctype_size = size_of::<Self::CType>();
        if total_len < ctype_size {
            return Err(SystemError::EINVAL);
        }

        let c_type_bytes = &buf[..ctype_size];
        // SAFETY:
        // - `c_type_bytes` has at least `size_of::<Self::CType>()` bytes (checked above).
        // - Netlink payload memory may be unaligned, so use `read_unaligned`.
        let c_type =
            unsafe { core::ptr::read_unaligned(c_type_bytes.as_ptr() as *const Self::CType) };
        // log::info!("c_type: {:?}", c_type);

        let total_len_with_padding = Self::total_len_with_padding();
        if total_len < total_len_with_padding {
            return Err(SystemError::EINVAL);
        }

        let Ok(body) = c_type.try_into() else {
            return Err(SystemError::EINVAL);
        };

        let remaining_len = total_len.saturating_sub(total_len_with_padding);

        Ok((body, remaining_len, total_len_with_padding))
    }

    fn write_to_buf(&self, buf: &mut Vec<u8>) -> Result<(), SystemError> {
        // log::info!("SegmentBody write_to_buf");
        let c_type = Self::CType::from(*self);

        let body_bytes = unsafe {
            core::slice::from_raw_parts(
                &c_type as *const Self::CType as *const u8,
                size_of::<Self::CType>(),
            )
        };
        buf.extend_from_slice(body_bytes);

        // let total_len_with_padding = Self::total_len_with_padding();
        let padding_len = Self::padding_len();

        if padding_len > 0 {
            buf.extend(vec![0u8; padding_len]);
        }

        Ok(())
    }

    fn total_len_with_padding() -> usize {
        let payload_len = size_of::<Self::CType>();
        align_up(payload_len, NLMSG_ALIGN)
    }

    fn padding_len() -> usize {
        let payload_len = size_of::<Self::CType>();
        Self::total_len_with_padding() - payload_len
    }
}
