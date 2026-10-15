//! Bounded live rtnetlink dumps. Retain a request and cursors, not a snapshot
//! of all devices, addresses, routes, or neighbors behind an idle reader.
use super::{addr, link, neigh, route};
use crate::{
    libs::mutex::Mutex,
    net::socket::{
        netlink::{
            message::{
                segment::{
                    ack::DoneSegment, header::GetRequestFlags, header::SegHdrCommonFlags,
                    CSegmentType,
                },
                ProtocolSegment,
            },
            receiver::MessageQueue,
            route::message::{segment::RouteNlSegment, RouteNlMessage, RouteNlPacket},
            table::{NetlinkRouteProtocol, SupportedNetlinkProtocol},
        },
        AddressFamily,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::{sync::Arc, vec::Vec};
use core::ops::Bound::{Included, Unbounded};
use smoltcp::wire::IpAddress;
use system_error::SystemError;

const PAGE_BYTES: usize = 4096;
const SCAN_BUDGET: usize = 128;

#[derive(Debug)]
pub struct RouteSocketState {
    dump: Mutex<Option<DumpSession>>,
}

impl Default for RouteSocketState {
    fn default() -> Self {
        Self {
            dump: Mutex::new(None),
        }
    }
}

#[derive(Debug)]
struct DumpSession {
    request: RouteNlSegment,
    position: Position,
}

#[derive(Debug, Clone, Copy)]
struct Position {
    key: usize,
    index: usize,
    family: u8,
    done: bool,
}

impl Default for Position {
    fn default() -> Self {
        Self {
            key: 0,
            index: 0,
            family: 2,
            done: false,
        }
    }
}

impl RouteSocketState {
    pub(super) fn is_dump(request: &RouteNlSegment) -> bool {
        matches!(
            request,
            RouteNlSegment::GetLink(_)
                | RouteNlSegment::GetAddr(_)
                | RouteNlSegment::GetRoute(_)
                | RouteNlSegment::GetNeigh(_)
        ) && GetRequestFlags::from_bits_truncate(request.header().flags)
            .contains(GetRequestFlags::DUMP)
    }

    pub(super) fn start(
        &self,
        request: &RouteNlSegment,
        port: u32,
        netns: &Arc<NetNamespace>,
        queue: &MessageQueue<RouteNlPacket>,
    ) -> Result<(), SystemError> {
        let mut slot = self.dump.lock();
        if slot.is_some() {
            return Err(SystemError::EBUSY);
        }
        match request {
            RouteNlSegment::GetLink(request) => link::validate_dumplink_request(request.body())?,
            RouteNlSegment::GetNeigh(request) => neigh::validate_dump(request)?,
            _ => {}
        }
        *slot = Some(DumpSession {
            request: request.clone(),
            position: Position::default(),
        });
        if let Err(error) = Self::drive(&mut slot, port, netns, queue) {
            *slot = None;
            return Err(error);
        }
        Ok(())
    }

    pub(in crate::net::socket::netlink) fn drive_if_queue_empty(
        &self,
        queue: &MessageQueue<RouteNlPacket>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) {
        let mut slot = self.dump.lock();
        if Self::drive(&mut slot, port, netns, queue).is_err() {
            *slot = None;
            NetlinkRouteProtocol::report_overrun(port, netns.clone());
        }
    }

    fn drive(
        slot: &mut Option<DumpSession>,
        port: u32,
        netns: &Arc<NetNamespace>,
        queue: &MessageQueue<RouteNlPacket>,
    ) -> Result<(), SystemError> {
        loop {
            if queue.is_closed() {
                *slot = None;
                return Ok(());
            }
            if !queue.is_empty() {
                return Ok(());
            }
            let Some(session) = slot.as_mut() else {
                return Ok(());
            };
            let (segments, position) = {
                let _rtnl = crate::net::rtnl::lock();
                session.page(netns)?
            };
            // A filter may consume a scan budget without producing a record.
            // Commit that progress, release RTNL and yield before continuing;
            // never strand a live dump in an empty queue awaiting recv().
            if segments.is_empty() {
                session.position = position;
                crate::sched::sched_yield();
                continue;
            }
            let packet = RouteNlPacket::kernel(RouteNlMessage::new(segments))?;
            match NetlinkRouteProtocol::enqueue_dump(port, packet, netns.clone()) {
                Ok(()) => {
                    session.position = position;
                    if position.done {
                        *slot = None;
                    }
                    return Ok(());
                }
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

impl DumpSession {
    fn page(
        &self,
        netns: &Arc<NetNamespace>,
    ) -> Result<(Vec<RouteNlSegment>, Position), SystemError> {
        let mut position = self.position;
        let mut segments = Vec::new();
        let mut length = 0;
        for _ in 0..SCAN_BUDGET {
            let previous = position;
            let Some(mut segment) = self.next(&mut position, netns)? else {
                if position.done {
                    break;
                }
                continue;
            };
            let size = segment.header().len as usize;
            if !segments.is_empty() && length + size > PAGE_BYTES {
                position = previous;
                break;
            }
            segment.header_mut().flags |= SegHdrCommonFlags::MULTI.bits();
            segments.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            segments.push(segment);
            length += size;
            if position.done || length >= PAGE_BYTES {
                break;
            }
        }
        Ok((segments, position))
    }

    fn done(&self, position: &mut Position) -> RouteNlSegment {
        position.done = true;
        RouteNlSegment::Done(DoneSegment::new_from_request(self.request.header(), None))
    }

    fn next(
        &self,
        position: &mut Position,
        netns: &Arc<NetNamespace>,
    ) -> Result<Option<RouteNlSegment>, SystemError> {
        match &self.request {
            RouteNlSegment::GetLink(request) => {
                let device = netns
                    .device_list()
                    .range((Included(position.key), Unbounded))
                    .next()
                    .map(|(&key, device)| (key, device.clone()));
                let Some((key, device)) = device else {
                    return Ok(Some(self.done(position)));
                };
                position.key = key.saturating_add(1);
                Ok(Some(RouteNlSegment::NewLink(link::iface_to_link_message(
                    request.header(),
                    CSegmentType::NEWLINK,
                    &device,
                )?)))
            }
            RouteNlSegment::GetAddr(request) => {
                let device = netns
                    .device_list()
                    .range((Included(position.key), Unbounded))
                    .next()
                    .map(|(&key, device)| (key, device.clone()));
                let Some((key, device)) = device else {
                    if position.family == 2 {
                        position.family = 10;
                        position.key = 0;
                        position.index = 0;
                        return Ok(None);
                    }
                    return Ok(Some(self.done(position)));
                };
                if key != position.key {
                    position.index = 0;
                }
                position.key = key;
                if request
                    .body()
                    .index
                    .is_some_and(|index| index.get() != key as u32)
                {
                    position.key = key.saturating_add(1);
                    position.index = 0;
                    return Ok(None);
                }
                let Some((cidr, label, broadcast)) =
                    crate::net::address::address_at(&device, position.index)?
                else {
                    position.key = key.saturating_add(1);
                    position.index = 0;
                    return Ok(None);
                };
                position.index += 1;
                let family = match cidr.address() {
                    IpAddress::Ipv4(_) => 2,
                    IpAddress::Ipv6(_) => 10,
                };
                let requested = AddressFamily::try_from(request.body().family as u16)
                    .ok()
                    .filter(|family| *family != AddressFamily::Unspecified);
                if family != position.family
                    || requested.is_some_and(|requested| requested as u8 != family)
                {
                    return Ok(None);
                }
                Ok(Some(RouteNlSegment::NewAddr(addr::addr_to_segment(
                    request.header(),
                    &device,
                    cidr,
                    CSegmentType::NEWADDR,
                    Some(&label),
                    broadcast,
                )?)))
            }
            RouteNlSegment::GetRoute(request) => {
                if request.header().type_ == CSegmentType::GETRULE as u16 {
                    return Ok(Some(self.done(position)));
                }
                let Some(entry) = crate::net::route::dump_entry(netns, position.index) else {
                    return Ok(Some(self.done(position)));
                };
                position.index += 1;
                let family = match entry.destination.address() {
                    IpAddress::Ipv4(_) => AddressFamily::INet,
                    IpAddress::Ipv6(_) => AddressFamily::INet6,
                };
                if request.body().family != AddressFamily::Unspecified
                    && request.body().family != family
                {
                    return Ok(None);
                }
                Ok(Some(RouteNlSegment::NewRoute(route::route_to_segment(
                    request.header(),
                    CSegmentType::NEWROUTE,
                    entry,
                    crate::net::socket::netlink::route::message::segment::route::RouteFlags::empty(
                    ),
                    false,
                    0,
                )?)))
            }
            RouteNlSegment::GetNeigh(request) => {
                let Some(entry) = crate::net::neighbor::dump_entry(netns, position.index) else {
                    return Ok(Some(self.done(position)));
                };
                position.index += 1;
                neigh::dump_entry_segment(request, entry)
            }
            _ => Err(SystemError::EINVAL),
        }
    }
}
