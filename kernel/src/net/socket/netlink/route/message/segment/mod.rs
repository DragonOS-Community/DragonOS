pub mod addr;
pub mod route;

use crate::net::socket::netlink::{
    message::{
        segment::{
            ack::{DoneSegment, ErrorSegment},
            header::CMsgSegHdr,
            CSegmentType,
        },
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
    Done(DoneSegment),
    Error(ErrorSegment),
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
            RouteNlSegment::Done(done_segment) => done_segment.header(),
            RouteNlSegment::Error(error_segment) => error_segment.header(),
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
            RouteNlSegment::Done(done_segment) => done_segment.header_mut(),
            RouteNlSegment::Error(error_segment) => error_segment.header_mut(),
        }
    }

    fn read_from(buf: &[u8]) -> Result<Self, SystemError> {
        let header_size = size_of::<CMsgSegHdr>();
        if buf.len() < header_size {
            log::warn!("the buffer is too small to read a netlink segment header");
            return Err(SystemError::EINVAL);
        }

        let header = unsafe { *(buf.as_ptr() as *const CMsgSegHdr) };
        let payload_buf = &buf[header_size..];

        let segment = match CSegmentType::try_from(header.type_)? {
            CSegmentType::GETADDR => {
                RouteNlSegment::GetAddr(AddrSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::GETROUTE => {
                RouteNlSegment::GetRoute(RouteSegment::read_from_buf(header, payload_buf)?)
            }
            _ => return Err(SystemError::EINVAL),
        };

        Ok(segment)
    }

    fn write_to(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        // log::info!("RouteNlSegment write_to");
        let copied = match self {
            RouteNlSegment::NewAddr(addr_segment) => addr_segment.write_to_buf(buf)?,
            RouteNlSegment::NewRoute(route_segment) => route_segment.write_to_buf(buf)?,
            RouteNlSegment::Done(done_segment) => done_segment.write_to_buf(buf)?,
            RouteNlSegment::Error(error_segment) => error_segment.write_to_buf(buf)?,
            _ => {
                log::warn!("write_to is not implemented for this segment type");
                return Err(SystemError::ENOSYS);
            }
        };

        Ok(copied)
    }

    fn protocol(&self) -> crate::net::socket::netlink::table::StandardNetlinkProtocol {
        crate::net::socket::netlink::table::StandardNetlinkProtocol::ROUTE
    }
}
