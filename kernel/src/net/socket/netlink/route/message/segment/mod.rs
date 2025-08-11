pub mod addr;
pub mod route;

use crate::net::socket::netlink::{
    message::{
        segment::{header::CMsgSegHdr, CSegmentType},
        ProtocolSegment,
    },
    route::message::segment::{addr::AddrSegment, route::RouteSegment},
};
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug)]
pub enum RouteNlSegment {
    // NewLink(LinkSegment),
    // GetLink(LinkSegment),
    NewAddr(AddrSegment),
    GetAddr(AddrSegment),
    // Done(DoneSegment),
    // Error(ErrorSegment),
    NewRoute(RouteSegment),
    DelRoute(RouteSegment),
    GetRoute(RouteSegment),
}

impl ProtocolSegment for RouteNlSegment {
    fn header(&self) -> &crate::net::socket::netlink::message::segment::header::CMsgSegHdr {
        match self {
            RouteNlSegment::NewRoute(route_segment)
            | RouteNlSegment::DelRoute(route_segment)
            | RouteNlSegment::GetRoute(route_segment) => route_segment.header(),
            RouteNlSegment::NewAddr(addr_segment) | RouteNlSegment::GetAddr(addr_segment) => {
                addr_segment.header()
            }
        }
    }

    fn header_mut(
        &mut self,
    ) -> &mut crate::net::socket::netlink::message::segment::header::CMsgSegHdr {
        match self {
            RouteNlSegment::NewRoute(route_segment)
            | RouteNlSegment::DelRoute(route_segment)
            | RouteNlSegment::GetRoute(route_segment) => route_segment.header_mut(),
            RouteNlSegment::NewAddr(addr_segment) | RouteNlSegment::GetAddr(addr_segment) => {
                addr_segment.header_mut()
            }
        }
    }

    fn read_from(buf: &[u8]) -> Result<Self, SystemError> {
        if buf.len() < size_of::<CMsgSegHdr>() {
            log::warn!("the buffer is too small to read a netlink segment header");
            return Err(SystemError::EINVAL);
        }

        let header = unsafe { *(buf.as_ptr() as *const CMsgSegHdr) };

        let segment = match CSegmentType::try_from(header.type_)? {
            CSegmentType::GETADDR => {
                RouteNlSegment::GetAddr(AddrSegment::read_from_buf(header, buf)?)
            }
            CSegmentType::GETROUTE => {
                RouteNlSegment::GetRoute(RouteSegment::read_from_buf(header, buf)?)
            }
            _ => return Err(SystemError::EINVAL),
        };

        Ok(segment)
    }

    fn write_to(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let mut kernel_buf: Vec<u8> = vec![];
        match self {
            RouteNlSegment::NewAddr(addr_segment) => addr_segment.write_to_buf(&mut kernel_buf)?,
            RouteNlSegment::NewRoute(route_segment) => {
                route_segment.write_to_buf(&mut kernel_buf)?
            }
            _ => {
                log::warn!("write_to is not implemented for this segment type");
                return Err(SystemError::ENOSYS);
            }
        }

        let actual_len = kernel_buf.len().min(buf.len());
        let copied = if !kernel_buf.is_empty() {
            buf[..actual_len].copy_from_slice(&kernel_buf[..actual_len]);
            actual_len
        } else {
            // 如果没有数据需要写入，返回0
            0
        };

        Ok(copied)
    }
}
