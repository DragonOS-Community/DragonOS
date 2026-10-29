pub(super) mod attr;
pub(super) mod segment;

use crate::net::socket::netlink::{
    addr::NetlinkSocketAddr, message::Message, route::message::segment::RouteNlSegment,
    table::MulticastMessage,
};
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

pub(in crate::net::socket::netlink) type RouteNlMessage = Message<RouteNlSegment>;

/// Retained datagrams own wire bytes rather than a typed response tree.
/// Multicast clones are charged to each subscriber, including entry overhead.
#[derive(Debug, Clone)]
pub struct RouteNlPacket {
    bytes: Arc<Vec<u8>>,
    source: NetlinkSocketAddr,
}

impl RouteNlPacket {
    pub fn kernel(message: RouteNlMessage) -> Result<Self, SystemError> {
        Self::notification(message, 0)
    }

    pub fn notification(message: RouteNlMessage, group: u32) -> Result<Self, SystemError> {
        let mut bytes = Vec::new();
        let length = message.total_len();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(length, 0);
        let written = message.write_to(&mut bytes)?;
        bytes.truncate(written);
        Ok(Self::user(
            Arc::try_new(bytes).map_err(|_| SystemError::ENOMEM)?,
            NetlinkSocketAddr::new(
                0,
                crate::net::socket::netlink::addr::multicast::GroupIdSet::new(group),
            ),
        ))
    }

    pub fn user(bytes: Arc<Vec<u8>>, source: NetlinkSocketAddr) -> Self {
        Self { bytes, source }
    }

    pub fn total_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn queue_charge(&self) -> usize {
        self.bytes
            .capacity()
            .saturating_add(crate::net::socket::netlink::receiver::MESSAGE_OVERHEAD)
    }

    pub fn source(&self) -> NetlinkSocketAddr {
        self.source
    }

    pub fn write_to(&self, writer: &mut [u8]) -> Result<usize, SystemError> {
        let length = self.bytes.len().min(writer.len());
        writer[..length].copy_from_slice(&self.bytes[..length]);
        Ok(length)
    }
}

impl MulticastMessage for RouteNlPacket {}
