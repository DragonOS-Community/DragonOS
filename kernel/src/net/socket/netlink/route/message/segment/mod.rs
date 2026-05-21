pub mod addr;
pub mod link;
pub mod neigh;
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
    route::message::{
        attr::{addr::AddrAttr, link::LinkAttr, neigh::NeighAttr, route::RouteAttr},
        segment::{
            addr::{AddrMessageFlags, AddrSegment, AddrSegmentBody, CIfaddrMsg, RtScope},
            link::{CIfinfoMsg, LinkMessageFlags, LinkSegment, LinkSegmentBody},
            neigh::{CNdMsg, NeighSegment, NeighSegmentBody, NeighState},
            route::{
                CRtMsg, RouteFlags, RouteProtocol, RouteScope, RouteSegment, RouteSegmentBody,
                RouteTable, RouteType,
            },
        },
    },
};
use crate::{
    driver::net::types::{InterfaceFlags, InterfaceType},
    net::socket::AddressFamily,
};
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug, Clone)]
pub enum RouteNlSegment {
    NewLink(LinkSegment),
    SetLink(LinkSegment),
    GetLink(LinkSegment),
    NewAddr(AddrSegment),
    DelAddr(AddrSegment),
    GetAddr(AddrSegment),
    Done(DoneSegment),
    Error(ErrorSegment),
    NewRoute(RouteSegment),
    DelRoute(RouteSegment),
    GetRoute(RouteSegment),
    NewNeigh(NeighSegment),
    DelNeigh(NeighSegment),
    GetNeigh(NeighSegment),
}

impl ProtocolSegment for RouteNlSegment {
    fn header(&self) -> &crate::net::socket::netlink::message::segment::header::CMsgSegHdr {
        match self {
            RouteNlSegment::NewRoute(route_segment)
            | RouteNlSegment::DelRoute(route_segment)
            | RouteNlSegment::GetRoute(route_segment) => route_segment.header(),
            RouteNlSegment::NewAddr(addr_segment)
            | RouteNlSegment::DelAddr(addr_segment)
            | RouteNlSegment::GetAddr(addr_segment) => addr_segment.header(),
            RouteNlSegment::NewLink(link_segment) | RouteNlSegment::GetLink(link_segment) => {
                link_segment.header()
            }
            RouteNlSegment::SetLink(link_segment) => link_segment.header(),
            RouteNlSegment::NewNeigh(neigh_segment)
            | RouteNlSegment::DelNeigh(neigh_segment)
            | RouteNlSegment::GetNeigh(neigh_segment) => neigh_segment.header(),
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
            RouteNlSegment::NewAddr(addr_segment)
            | RouteNlSegment::DelAddr(addr_segment)
            | RouteNlSegment::GetAddr(addr_segment) => addr_segment.header_mut(),
            RouteNlSegment::NewLink(link_segment) | RouteNlSegment::GetLink(link_segment) => {
                link_segment.header_mut()
            }
            RouteNlSegment::SetLink(link_segment) => link_segment.header_mut(),
            RouteNlSegment::NewNeigh(neigh_segment)
            | RouteNlSegment::DelNeigh(neigh_segment)
            | RouteNlSegment::GetNeigh(neigh_segment) => neigh_segment.header_mut(),
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

        // SAFETY:
        // - `buf` has at least `size_of::<CMsgSegHdr>()` bytes (checked above).
        // - Netlink input buffer alignment is not guaranteed, so use `read_unaligned`.
        let header = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const CMsgSegHdr) };
        let segment_len = header.len as usize;
        if segment_len < header_size || buf.len() < segment_len {
            return Err(SystemError::EINVAL);
        }
        let payload_len = segment_len - header_size;
        let payload_buf = &buf[header_size..segment_len];

        let segment = match CSegmentType::try_from(header.type_)? {
            CSegmentType::NEWADDR => {
                RouteNlSegment::NewAddr(AddrSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::DELADDR => {
                RouteNlSegment::DelAddr(AddrSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::GETADDR => {
                if payload_len < size_of::<CIfaddrMsg>() {
                    RouteNlSegment::GetAddr(read_short_getaddr_segment(header, payload_buf)?)
                } else {
                    RouteNlSegment::GetAddr(AddrSegment::read_from_buf(header, payload_buf)?)
                }
            }
            CSegmentType::GETROUTE => {
                if payload_len < size_of::<CRtMsg>() {
                    RouteNlSegment::GetRoute(read_short_getroute_segment(header, payload_buf)?)
                } else {
                    RouteNlSegment::GetRoute(RouteSegment::read_from_buf(header, payload_buf)?)
                }
            }
            CSegmentType::NEWROUTE => {
                RouteNlSegment::NewRoute(RouteSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::DELROUTE => {
                RouteNlSegment::DelRoute(RouteSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::GETLINK => {
                if payload_len < size_of::<CIfinfoMsg>() {
                    RouteNlSegment::GetLink(read_short_getlink_segment(header, payload_buf)?)
                } else {
                    RouteNlSegment::GetLink(LinkSegment::read_from_buf(header, payload_buf)?)
                }
            }
            CSegmentType::SETLINK | CSegmentType::NEWLINK | CSegmentType::DELLINK => {
                RouteNlSegment::SetLink(LinkSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::GETRULE => {
                if payload_len < size_of::<CRtMsg>() {
                    RouteNlSegment::GetRoute(read_short_getroute_segment(header, payload_buf)?)
                } else {
                    RouteNlSegment::GetRoute(RouteSegment::read_from_buf(header, payload_buf)?)
                }
            }
            CSegmentType::NEWNEIGH => {
                RouteNlSegment::NewNeigh(NeighSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::DELNEIGH => {
                RouteNlSegment::DelNeigh(NeighSegment::read_from_buf(header, payload_buf)?)
            }
            CSegmentType::GETNEIGH => {
                if payload_len < size_of::<CNdMsg>() {
                    RouteNlSegment::GetNeigh(read_short_getneigh_segment(header, payload_buf)?)
                } else {
                    RouteNlSegment::GetNeigh(NeighSegment::read_from_buf(header, payload_buf)?)
                }
            }
            _ => return Err(SystemError::EINVAL),
        };

        Ok(segment)
    }

    fn write_to(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        // log::info!("RouteNlSegment write_to");
        let copied = match self {
            RouteNlSegment::NewAddr(addr_segment) | RouteNlSegment::DelAddr(addr_segment) => {
                addr_segment.write_to_buf(buf)?
            }
            RouteNlSegment::NewRoute(route_segment) | RouteNlSegment::DelRoute(route_segment) => {
                route_segment.write_to_buf(buf)?
            }
            RouteNlSegment::NewLink(link_segment) => link_segment.write_to_buf(buf)?,
            RouteNlSegment::NewNeigh(neigh_segment)
            | RouteNlSegment::DelNeigh(neigh_segment)
            | RouteNlSegment::GetNeigh(neigh_segment) => neigh_segment.write_to_buf(buf)?,
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

fn read_short_getlink_segment(
    header: CMsgSegHdr,
    payload: &[u8],
) -> Result<LinkSegment, SystemError> {
    let family = read_rtgen_family(payload)?;
    let body = LinkSegmentBody {
        family,
        type_: InterfaceType::NETROM,
        index: None,
        flags: InterfaceFlags::empty(),
        change: LinkMessageFlags::empty(),
        pad: None,
    };
    let mut segment = LinkSegment::new(header, body, Vec::<LinkAttr>::new());
    segment.header_mut().len = header.len;
    Ok(segment)
}

fn read_short_getaddr_segment(
    header: CMsgSegHdr,
    payload: &[u8],
) -> Result<AddrSegment, SystemError> {
    let family = read_rtgen_family(payload)?;
    let body = AddrSegmentBody {
        family: family as i32,
        prefix_len: 0,
        flags: AddrMessageFlags::empty(),
        scope: RtScope::UNIVERSE,
        index: None,
    };
    let mut segment = AddrSegment::new(header, body, Vec::<AddrAttr>::new());
    segment.header_mut().len = header.len;
    Ok(segment)
}

fn read_short_getroute_segment(
    header: CMsgSegHdr,
    payload: &[u8],
) -> Result<RouteSegment, SystemError> {
    let family = read_rtgen_family(payload)?;
    let body = RouteSegmentBody {
        family,
        dst_len: 0,
        src_len: 0,
        tos: 0,
        table: RouteTable::Unspec,
        protocol: RouteProtocol::Unspec,
        scope: RouteScope::Universe,
        type_: RouteType::Unspec,
        flags: RouteFlags::empty(),
    };
    let mut segment = RouteSegment::new(header, body, Vec::<RouteAttr>::new());
    segment.header_mut().len = header.len;
    Ok(segment)
}

#[allow(dead_code)]
fn read_short_getneigh_segment(
    header: CMsgSegHdr,
    payload: &[u8],
) -> Result<NeighSegment, SystemError> {
    let family = read_rtgen_family(payload)?;
    let body = NeighSegmentBody {
        family,
        ifindex: 0,
        state: NeighState::empty(),
        flags: 0,
        kind: RouteType::Unspec,
    };
    let mut segment = NeighSegment::new(header, body, Vec::<NeighAttr>::new());
    segment.header_mut().len = header.len;
    Ok(segment)
}

fn read_rtgen_family(payload: &[u8]) -> Result<AddressFamily, SystemError> {
    let family = payload.first().copied().ok_or(SystemError::EINVAL)?;
    AddressFamily::try_from(family as u16).map_err(|_| SystemError::EINVAL)
}
