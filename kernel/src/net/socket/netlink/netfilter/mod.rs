//! NFNetlink transport without registered filtering subsystems.
//!
//! This is deliberately not an nftables implementation. Valid subsystem
//! requests receive Linux's missing-subsystem errors, never a fake ruleset.

mod bound;

use super::{
    addr::NetlinkSocketAddr,
    common::NetlinkSocket,
    receiver::MessageQueue,
    table::{
        NetlinkKernelSocket, NetlinkNetfilterProtocol, ProtocolSocketTable,
        StandardNetlinkProtocol, SupportedNetlinkProtocol,
    },
};
use crate::{
    libs::rwsem::RwSem,
    process::{
        cred::{CAPFlags, Cred},
        namespace::net_namespace::NetNamespace,
        ProcessManager,
    },
};
use alloc::{sync::Arc, vec::Vec};
use core::any::Any;
use system_error::SystemError;

pub(super) type NetlinkNetfilterSocket = NetlinkSocket<NetlinkNetfilterProtocol>;

const RECEIVE_BUDGET: usize = 212_992;
const MAX_DATAGRAM: usize = RECEIVE_BUDGET - 32;
const HEADER_LEN: usize = 16;
const NFGEN_LEN: usize = 4;
const BATCH_BEGIN: u16 = 16;
const SUBSYSTEM_COUNT: u16 = 13;

#[derive(Debug)]
pub struct NetfilterMessage(Vec<u8>);

impl NetfilterMessage {
    fn charge(&self) -> usize {
        self.0.capacity().max(1)
    }
}

fn require_net_admin(netns: &NetNamespace, cred: &Cred) -> Result<(), SystemError> {
    if cred.has_capability_in_ns(netns.user_ns(), CAPFlags::CAP_NET_ADMIN) {
        Ok(())
    } else {
        Err(SystemError::EPERM)
    }
}

impl SupportedNetlinkProtocol for NetlinkNetfilterProtocol {
    type Message = NetfilterMessage;

    fn multicast_group_count() -> u32 {
        32
    }

    fn socket_table(netns: Arc<NetNamespace>) -> Arc<RwSem<ProtocolSocketTable<Self::Message>>> {
        netns.netlink_socket_table().netfilter()
    }

    fn new_message_queue() -> MessageQueue<Self::Message> {
        MessageQueue::with_limits(128, RECEIVE_BUDGET, NetfilterMessage::charge)
    }

    fn max_send_len() -> Option<usize> {
        Some(MAX_DATAGRAM)
    }

    fn max_recv_len() -> Option<usize> {
        Some(RECEIVE_BUDGET)
    }

    fn check_bind(addr: &NetlinkSocketAddr, netns: &Arc<NetNamespace>) -> Result<(), SystemError> {
        if !addr.groups().is_empty() {
            Self::check_membership(netns)?;
        }
        Ok(())
    }

    fn check_connect(
        addr: &NetlinkSocketAddr,
        netns: &Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        if addr.port() != 0 || !addr.groups().is_empty() {
            Self::check_membership(netns)?;
            // User-to-user and userspace multicast delivery are not provided
            // by this kernel-endpoint foundation.
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        Ok(())
    }

    fn check_membership(netns: &Arc<NetNamespace>) -> Result<(), SystemError> {
        require_net_admin(netns, &ProcessManager::current_pcb().cred())
    }
}

#[derive(Debug)]
pub(super) struct NetfilterKernelSocket;

impl NetlinkKernelSocket for NetfilterKernelSocket {
    fn protocol(&self) -> StandardNetlinkProtocol {
        StandardNetlinkProtocol::NETFILTER
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A borrowed, checked nlmsghdr. Keeping the original bytes also preserves the
/// request verbatim in NLMSG_ERROR, including its untrusted nlmsg_pid.
struct Request<'a> {
    bytes: &'a [u8],
}

impl<'a> Request<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let len = u32::from_ne_bytes(bytes[..4].try_into().ok()?) as usize;
        if len < HEADER_LEN || len > bytes.len() {
            return None;
        }
        Some(Self {
            bytes: &bytes[..len],
        })
    }
    fn kind(&self) -> u16 {
        u16::from_ne_bytes(self.bytes[4..6].try_into().unwrap())
    }
    fn flags(&self) -> u16 {
        u16::from_ne_bytes(self.bytes[6..8].try_into().unwrap())
    }

    fn reply(
        &self,
        error: Option<SystemError>,
        port: u32,
    ) -> Result<NetfilterMessage, SystemError> {
        let copied = if error.is_some() {
            self.bytes.len()
        } else {
            HEADER_LEN
        };
        // Linux nlmsg_append pads the echoed payload before nlmsg_end sets
        // the outer length. Preserve the embedded header's original length.
        let len = HEADER_LEN + 4 + ((copied + 3) & !3);
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.extend_from_slice(&(len as u32).to_ne_bytes());
        bytes.extend_from_slice(&2u16.to_ne_bytes()); // NLMSG_ERROR
        bytes.extend_from_slice(&(if error.is_none() { 0x100u16 } else { 0 }).to_ne_bytes());
        bytes.extend_from_slice(&self.bytes[8..12]); // sequence
        bytes.extend_from_slice(&port.to_ne_bytes()); // actual sender, not nlmsg_pid
        bytes.extend_from_slice(&error.map_or(0, |err| err.to_posix_errno()).to_ne_bytes());
        bytes.extend_from_slice(&self.bytes[..copied]);
        bytes.resize(len, 0);
        Ok(NetfilterMessage(bytes))
    }
}

impl NetfilterKernelSocket {
    fn request(
        &self,
        bytes: &[u8],
        port: u32,
        netns: Arc<NetNamespace>,
        opener: &Cred,
        explicit: bool,
    ) -> Result<(), SystemError> {
        // Linux checks framing and authorization once per input datagram,
        // before the ordinary REQUEST/control-message handling.
        let Some(first) = Request::parse(bytes) else {
            return Ok(());
        };
        let sender = ProcessManager::current_pcb().cred();
        if require_net_admin(&netns, &sender).is_err()
            || (!explicit && require_net_admin(&netns, opener).is_err())
        {
            return Self::ack(&first, Some(SystemError::EPERM), port, netns);
        }
        if first.kind() == BATCH_BEGIN {
            if bytes.len() < HEADER_LEN + NFGEN_LEN {
                return Ok(());
            }
            let error = batch_error(&first, bytes);
            return Self::ack(&first, Some(error), port, netns);
        }

        let mut remaining = bytes;
        while let Some(request) = Request::parse(remaining) {
            let error = if request.flags() & 1 != 0
                && request.kind() >= 16
                && request.bytes.len() >= HEADER_LEN + NFGEN_LEN
            {
                // No nf_tables, conntrack or compatibility subsystem has
                // registered a callback. Linux nfnetlink_rcv_msg: EINVAL.
                Some(SystemError::EINVAL)
            } else {
                None
            };
            if error.is_some() || request.flags() & 4 != 0 {
                Self::ack(&request, error, port, netns.clone())?;
            }
            let advance = ((request.bytes.len() + 3) & !3).min(remaining.len());
            remaining = &remaining[advance..];
        }
        Ok(())
    }

    fn ack(
        request: &Request<'_>,
        error: Option<SystemError>,
        port: u32,
        netns: Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        let reply = match request.reply(error, port) {
            Ok(reply) => reply,
            Err(SystemError::ENOMEM) => {
                NetlinkNetfilterProtocol::report_overrun(port, netns);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        match NetlinkNetfilterProtocol::unicast(port, reply, netns) {
            // Kernel replies do not propagate receive-queue exhaustion back
            // through sendmsg. The bounded queue reports ENOBUFS to its reader.
            Err(SystemError::ENOBUFS) => Ok(()),
            result => result,
        }
    }
}

fn batch_error(request: &Request<'_>, datagram: &[u8]) -> SystemError {
    // nla_parse_deprecated accepts unknown attributes and trailing padding,
    // but validates the one supported batch attribute's minimum payload.
    let mut attributes = request.bytes.get(HEADER_LEN + NFGEN_LEN..).unwrap_or(&[]);
    while attributes.len() >= 4 {
        let len = u16::from_ne_bytes(attributes[..2].try_into().unwrap()) as usize;
        if len < 4 || len > attributes.len() {
            break;
        }
        let kind = u16::from_ne_bytes(attributes[2..4].try_into().unwrap()) & 0x3fff;
        if kind == 1 && len < 8 {
            return SystemError::ERANGE;
        }
        attributes = &attributes[((len + 3) & !3).min(attributes.len())..];
    }
    let raw = [datagram[18], datagram[19]];
    let subsystem = if u16::from_ne_bytes(raw) == 10 {
        10
    } else {
        u16::from_be_bytes(raw)
    };
    if subsystem >= SUBSYSTEM_COUNT {
        SystemError::EINVAL
    } else {
        SystemError::EOPNOTSUPP_OR_ENOTSUP
    }
}
