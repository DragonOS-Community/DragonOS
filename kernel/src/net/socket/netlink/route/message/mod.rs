pub(super) mod attr;
pub(super) mod segment;

use crate::net::socket::netlink::{
    addr::NetlinkSocketAddr, message::Message, route::message::segment::RouteNlSegment,
    table::MulticastMessage,
};
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

pub(in crate::net::socket::netlink) type RouteNlMessage = Message<RouteNlSegment>;

/// A received rtnetlink datagram. Kernel replies are serialized from typed
/// segments; userspace delivery retains the exact bytes supplied to sendmsg.
#[derive(Debug, Clone)]
pub enum RouteNlPacket {
    Kernel {
        message: RouteNlMessage,
        group: u32,
    },
    User {
        bytes: Arc<Vec<u8>>,
        source: NetlinkSocketAddr,
    },
}

impl RouteNlPacket {
    pub fn kernel(message: RouteNlMessage) -> Self {
        Self::Kernel { message, group: 0 }
    }

    pub fn notification(message: RouteNlMessage, group: u32) -> Self {
        Self::Kernel { message, group }
    }

    pub fn user(bytes: Arc<Vec<u8>>, source: NetlinkSocketAddr) -> Self {
        Self::User { bytes, source }
    }

    pub fn total_len(&self) -> usize {
        match self {
            Self::Kernel { message, .. } => message.total_len(),
            Self::User { bytes, .. } => bytes.len(),
        }
    }

    /// Limit newly supported userspace delivery without changing the
    /// pre-existing, potentially multipart rtnetlink dump reply path. Include
    /// a conservative per-entry charge so tiny datagrams cannot fill the
    /// queue with hundreds of thousands of Arc/VecDeque nodes.
    pub fn queue_charge(&self) -> usize {
        match self {
            Self::Kernel { .. } => 0,
            Self::User { bytes, .. } => bytes.len().saturating_add(512),
        }
    }

    pub fn source(&self) -> NetlinkSocketAddr {
        match self {
            Self::Kernel { group, .. } => NetlinkSocketAddr::new(
                0,
                crate::net::socket::netlink::addr::multicast::GroupIdSet::new(*group),
            ),
            Self::User { source, .. } => *source,
        }
    }

    pub fn write_to(&self, writer: &mut [u8]) -> Result<usize, SystemError> {
        match self {
            Self::Kernel { message, .. } => message.write_to(writer),
            Self::User { bytes, .. } => {
                let copy_len = bytes.len().min(writer.len());
                writer[..copy_len].copy_from_slice(&bytes[..copy_len]);
                Ok(copy_len)
            }
        }
    }
}

impl MulticastMessage for RouteNlPacket {}
