//! NFNetlink transport and the supported nftables control messages.
//!
//! Unsupported rule operations still fail: only rules with an executable
//! packet-path representation may be committed.

mod bound;
mod ctnetlink;
mod nft;

use super::{
    addr::NetlinkSocketAddr,
    common::NetlinkSocket,
    receiver::MessageQueue,
    table::MulticastMessage,
    table::{
        NetlinkKernelSocket, NetlinkNetfilterProtocol, ProtocolSocketTable,
        StandardNetlinkProtocol, SupportedNetlinkProtocol,
    },
};
use crate::{
    libs::rwsem::RwSem,
    net::nftables::{
        NewSetSpec, NftChain, NftChainType, NftExpressionInput, NftRule, NftSet,
        NftSetElementInput, NftTable, NftVerdict,
    },
    process::{
        cred::{CAPFlags, Cred},
        namespace::net_namespace::NetNamespace,
        ProcessManager,
    },
};
use alloc::{sync::Arc, vec::Vec};
use core::any::Any;
use nft::{
    chain_attrs, chain_message, compat_match_reply, rule_attrs, rule_message, set_attrs,
    set_elem_attrs, set_elem_message, set_message, table_attrs, table_message,
    xt_addrtype_compatible, xt_conntrack_compatible, xt_tcp_compatible, GetTableResult,
    NftSocketState, NlaIter, SetElementMessageMeta,
};
use system_error::SystemError;

pub(super) type NetlinkNetfilterSocket = NetlinkSocket<NetlinkNetfilterProtocol>;

const RECEIVE_BUDGET: usize = 212_992;
const MAX_DATAGRAM: usize = RECEIVE_BUDGET - 32;
const HEADER_LEN: usize = 16;
const NFGEN_LEN: usize = 4;
const BATCH_BEGIN: u16 = 16;
const BATCH_END: u16 = 17;
const SUBSYSTEM_COUNT: u16 = 13;
const NFT_MSG_NEWTABLE: u16 = 10 << 8;
const NFT_MSG_GETTABLE: u16 = (10 << 8) | 1;
const NFT_MSG_DELTABLE: u16 = (10 << 8) | 2;
const NFT_MSG_NEWCHAIN: u16 = (10 << 8) | 3;
const NFT_MSG_GETCHAIN: u16 = (10 << 8) | 4;
const NFT_MSG_DELCHAIN: u16 = (10 << 8) | 5;
const NFT_MSG_NEWRULE: u16 = (10 << 8) | 6;
const NFT_MSG_GETRULE: u16 = (10 << 8) | 7;
const NFT_MSG_DELRULE: u16 = (10 << 8) | 8;
const NFT_MSG_NEWSET: u16 = (10 << 8) | 9;
const NFT_MSG_GETSET: u16 = (10 << 8) | 10;
const NFT_MSG_DELSET: u16 = (10 << 8) | 11;
const NFT_MSG_NEWSETELEM: u16 = (10 << 8) | 12;
const NFT_MSG_GETSETELEM: u16 = (10 << 8) | 13;
const NFT_MSG_DELSETELEM: u16 = (10 << 8) | 14;
const NFT_MSG_NEWGEN: u16 = (10 << 8) | 15;
const NFT_MSG_GETGEN: u16 = (10 << 8) | 16;
const NFT_MSG_GETFLOWTABLE: u16 = (10 << 8) | 23;
const NFT_COMPAT_GET: u16 = 11 << 8;
const NFNLGRP_NFTABLES: u32 = 7;
const NLM_F_ECHO: u16 = 8;

enum NotificationObject {
    Table(Arc<NftTable>),
    Chain(Arc<NftTable>, Arc<NftChain>),
    Rule(Arc<NftTable>, Arc<NftChain>, Arc<NftRule>),
    Set(Arc<NftTable>, Arc<NftSet>),
    /// Prebuilt with the element's value *at the operation*, not a retained
    /// full set/table snapshot for every update in a batch.
    SetElement(NetfilterMessage),
    /// Keep a deleted subtree as one event until commit decides whether any
    /// listener or echo recipient needs its descendant notifications.
    DeletedTableTree(Arc<NftTable>),
}

struct NftNotification {
    object: NotificationObject,
    kind: u16,
    sequence: u32,
    flags: u16,
    echo: bool,
}

#[derive(Clone, Debug)]
pub struct NetfilterMessage(Arc<Vec<u8>>);

impl NetfilterMessage {
    fn new(bytes: Vec<u8>) -> Result<Self, SystemError> {
        Arc::try_new(bytes)
            .map(Self)
            .map_err(|_| SystemError::ENOMEM)
    }
}

fn set_element_events(
    table: &NftTable,
    set: &NftSet,
    inputs: &[NftSetElementInput<'_>],
    sequence: u32,
    port: u32,
    kind: u16,
) -> Result<Vec<NetfilterMessage>, SystemError> {
    let mut events = Vec::new();
    events
        .try_reserve(inputs.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for input in inputs {
        let index = set
            .element_index(input.key, input.flags)
            .ok_or(SystemError::ENOENT)?;
        let stored = &set.elements[index];
        events.push(set_elem_message(
            table,
            set,
            &stored.key,
            stored.value.as_deref(),
            stored.verdict,
            stored.flags,
            SetElementMessageMeta {
                generation: 0,
                sequence,
                port,
                kind,
                flags: 0,
            },
        )?);
    }
    Ok(events)
}

impl MulticastMessage for NetfilterMessage {}

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
    type SocketState = NftSocketState;

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
#[derive(Clone, Copy)]
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
        NetfilterMessage::new(bytes)
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
        socket_state: &NftSocketState,
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
            if first.bytes.len() < HEADER_LEN + NFGEN_LEN {
                return Ok(());
            }
            let error = batch_error(&first);
            if error != SystemError::EOPNOTSUPP_OR_ENOTSUP {
                return Self::ack(&first, Some(error), port, netns);
            }
            let raw = [first.bytes[18], first.bytes[19]];
            if u16::from_be_bytes(raw) != 10 && u16::from_ne_bytes(raw) != 10 {
                return Self::ack(&first, Some(error), port, netns);
            }
            return Self::batch(&first, bytes, port, netns);
        }

        let mut remaining = bytes;
        while let Some(request) = Request::parse(remaining) {
            let mut dump_started = false;
            let error = if request.flags() & 1 != 0
                && request.kind() >= 16
                && request.bytes.len() >= HEADER_LEN + NFGEN_LEN
            {
                if request.kind() >> 8 == 1 {
                    match ctnetlink::request(&request, port, &netns, socket_state) {
                        Ok(GetTableResult::Replied) => None,
                        Ok(GetTableResult::DumpStarted) => {
                            dump_started = true;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else if request.kind() == NFT_MSG_GETGEN {
                    Self::send_generation(&request, port, &netns).err()
                } else if request.kind() == NFT_COMPAT_GET {
                    compat_match_reply(&request, port)
                        .and_then(|message| {
                            NetlinkNetfilterProtocol::unicast(port, message, netns.clone())
                        })
                        .err()
                } else if request.kind() == NFT_MSG_GETTABLE {
                    match socket_state.get_table(&request, port, &netns) {
                        Ok(GetTableResult::Replied) => None,
                        Ok(GetTableResult::DumpStarted) => {
                            dump_started = true;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else if request.kind() == NFT_MSG_GETCHAIN {
                    match socket_state.get_chain(&request, port, &netns) {
                        Ok(GetTableResult::Replied) => None,
                        Ok(GetTableResult::DumpStarted) => {
                            dump_started = true;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else if request.kind() == NFT_MSG_GETRULE {
                    match socket_state.get_rule(&request, port, &netns) {
                        Ok(GetTableResult::Replied) => None,
                        Ok(GetTableResult::DumpStarted) => {
                            dump_started = true;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else if request.kind() == NFT_MSG_GETSET {
                    match socket_state.get_set(&request, port, &netns) {
                        Ok(GetTableResult::Replied) => None,
                        Ok(GetTableResult::DumpStarted) => {
                            dump_started = true;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else if request.kind() == NFT_MSG_GETSETELEM {
                    match socket_state.get_set_elements(&request, port, &netns) {
                        Ok(GetTableResult::Replied) => None,
                        Ok(GetTableResult::DumpStarted) => {
                            dump_started = true;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else if request.kind() == NFT_MSG_GETFLOWTABLE {
                    match socket_state.get_empty_object(&request, port, &netns) {
                        Ok(GetTableResult::Replied) => None,
                        Ok(GetTableResult::DumpStarted) => {
                            dump_started = true;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else {
                    // Never acknowledge objects which the packet path cannot enforce.
                    Some(SystemError::EINVAL)
                }
            } else {
                None
            };
            if error.is_some() || (!dump_started && request.flags() & 4 != 0) {
                Self::ack(&request, error, port, netns.clone())?;
            }
            let advance = ((request.bytes.len() + 3) & !3).min(remaining.len());
            remaining = &remaining[advance..];
        }
        Ok(())
    }

    fn batch(
        begin: &Request<'_>,
        datagram: &[u8],
        port: u32,
        netns: Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        let mut generation = 0;
        for nla in NlaIter::new(&begin.bytes[HEADER_LEN + NFGEN_LEN..]) {
            let nla = match nla {
                Ok(nla) => nla,
                Err(error) => return Self::ack(begin, Some(error), port, netns),
            };
            if nla.kind == 1 {
                if nla.value.len() < 4 {
                    return Self::ack(begin, Some(SystemError::ERANGE), port, netns);
                }
                generation = u32::from_be_bytes(nla.value[..4].try_into().unwrap());
            }
        }
        let mut transaction = match netns.nftables().transaction(generation) {
            Ok(transaction) => transaction,
            Err(error) => return Self::ack(begin, Some(error), port, netns.clone()),
        };
        let mut replies = Vec::new();
        let mut notifications = Vec::new();
        let mut failed = false;
        let mut end_seen = false;
        let mut framing_error = false;
        let mut remaining = &datagram[((begin.bytes.len() + 3) & !3).min(datagram.len())..];
        while let Some(request) = Request::parse(remaining) {
            if request.bytes.len() < HEADER_LEN + NFGEN_LEN {
                failed = true;
                framing_error = true;
                break;
            }
            if request.flags() & 1 == 0 {
                failed = true;
                replies.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                replies.push((request, Some(SystemError::EINVAL)));
                let advance = ((request.bytes.len() + 3) & !3).min(remaining.len());
                remaining = &remaining[advance..];
                continue;
            }
            if request.kind() == BATCH_BEGIN {
                failed = true;
                framing_error = true;
                break;
            }
            if request.kind() == BATCH_END {
                end_seen = true;
                break;
            }
            let family = request.bytes[HEADER_LEN];
            let error = match request.kind() {
                NFT_MSG_NEWTABLE => table_attrs(&request)
                    .and_then(|attrs| {
                        let created = transaction.new_table(
                            family,
                            attrs.name.ok_or(SystemError::EINVAL)?,
                            attrs.flags,
                            attrs.userdata,
                            request.flags() & 0x200 != 0,
                            request.flags() & 0x100 != 0,
                        )?;
                        if let Some(table) = created {
                            notifications
                                .try_reserve(1)
                                .map_err(|_| SystemError::ENOMEM)?;
                            notifications.push(NftNotification {
                                object: NotificationObject::Table(table),
                                kind: NFT_MSG_NEWTABLE,
                                sequence: u32::from_ne_bytes(
                                    request.bytes[8..12].try_into().unwrap(),
                                ),
                                flags: request.flags() & (0x400 | 0x200),
                                echo: request.flags() & NLM_F_ECHO != 0,
                            });
                        }
                        Ok(())
                    })
                    .err(),
                NFT_MSG_NEWCHAIN => chain_attrs(&request)
                    .and_then(|attrs| {
                        let table_name = attrs.table.ok_or(SystemError::EINVAL)?;
                        let name = attrs.name.ok_or(SystemError::EINVAL)?;
                        if !matches!(family, 1 | 2 | 10)
                            || attrs.handle.is_some()
                            || attrs.flags != 0
                        {
                            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                        }
                        let hook = match attrs.hook {
                            Some((hooknum @ 0..=4, priority)) => {
                                let chain_type = match attrs.chain_type {
                                    Some(b"filter") => NftChainType::Filter,
                                    Some(b"nat") if hooknum != 2 && priority > -200 => {
                                        NftChainType::Nat
                                    }
                                    _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                                };
                                let hook = match hooknum {
                                    0 => crate::net::nftables::NftIpv4Hook::PreRouting,
                                    1 => crate::net::nftables::NftIpv4Hook::LocalIn,
                                    2 => crate::net::nftables::NftIpv4Hook::Forward,
                                    3 => crate::net::nftables::NftIpv4Hook::LocalOut,
                                    _ => crate::net::nftables::NftIpv4Hook::PostRouting,
                                };
                                Some((
                                    hook,
                                    priority,
                                    attrs.policy.unwrap_or(NftVerdict::Accept),
                                    chain_type,
                                ))
                            }
                            // A regular chain has no hook; Linux accepts but
                            // does not use its optional chain type attribute.
                            None if attrs.policy.is_none() => None,
                            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                        };
                        let created = transaction.new_ip_chain(
                            family,
                            table_name,
                            name,
                            hook,
                            request.flags() & 0x200 != 0,
                            request.flags() & 0x100 != 0,
                        )?;
                        if let Some((table, chain)) = created {
                            notifications
                                .try_reserve(1)
                                .map_err(|_| SystemError::ENOMEM)?;
                            notifications.push(NftNotification {
                                object: NotificationObject::Chain(table, chain),
                                kind: NFT_MSG_NEWCHAIN,
                                sequence: u32::from_ne_bytes(
                                    request.bytes[8..12].try_into().unwrap(),
                                ),
                                flags: request.flags() & (0x400 | 0x200),
                                echo: request.flags() & NLM_F_ECHO != 0,
                            });
                        }
                        Ok(())
                    })
                    .err(),
                NFT_MSG_NEWRULE => rule_attrs(&request)
                    .and_then(|attrs| {
                        if !matches!(family, 1 | 2 | 10)
                            || attrs.handle.is_some()
                            || request.flags() & 0x400 == 0
                            || request.flags() & 0x100 != 0
                        {
                            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                        }
                        let expressions = attrs.expressions.ok_or(SystemError::EINVAL)?;
                        if expressions.iter().any(|expression| match expression {
                            NftExpressionInput::XtTcp(_) => family != 2,
                            NftExpressionInput::XtAddrtype(_) => family == 1,
                            _ => false,
                        }) {
                            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                        }
                        if family == 1
                            && expressions.iter().any(|expression| {
                                matches!(expression, NftExpressionInput::XtConntrack { .. })
                            })
                        {
                            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                        }
                        if expressions
                            .iter()
                            .any(|expression| matches!(expression, NftExpressionInput::XtTcp(_)))
                        {
                            xt_tcp_compatible(attrs.compat)?;
                        }
                        if expressions.iter().any(|expression| {
                            matches!(expression, NftExpressionInput::XtAddrtype(_))
                        }) {
                            xt_addrtype_compatible(attrs.compat)?;
                        }
                        if expressions.iter().any(|expression| {
                            matches!(expression, NftExpressionInput::XtConntrack { .. })
                        }) {
                            xt_conntrack_compatible(attrs.compat)?;
                        }
                        let (table, chain, rule) = transaction.new_ip_rule(
                            family,
                            attrs.table.ok_or(SystemError::EINVAL)?,
                            attrs.chain.ok_or(SystemError::EINVAL)?,
                            &expressions,
                            request.flags() & 0x800 != 0,
                            attrs.position,
                        )?;
                        notifications
                            .try_reserve(1)
                            .map_err(|_| SystemError::ENOMEM)?;
                        notifications.push(NftNotification {
                            object: NotificationObject::Rule(table, chain, rule),
                            kind: NFT_MSG_NEWRULE,
                            sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
                            flags: request.flags() & (0x400 | 0x200 | 0x800),
                            echo: request.flags() & NLM_F_ECHO != 0,
                        });
                        Ok(())
                    })
                    .err(),
                NFT_MSG_DELRULE => rule_attrs(&request)
                    .and_then(|attrs| {
                        let removed = transaction.del_rules(
                            family,
                            attrs.table.ok_or(SystemError::EINVAL)?,
                            attrs.chain,
                            attrs.handle,
                        )?;
                        notifications
                            .try_reserve(removed.len())
                            .map_err(|_| SystemError::ENOMEM)?;
                        for (table, chain, rule) in removed {
                            notifications.push(NftNotification {
                                object: NotificationObject::Rule(table, chain, rule),
                                kind: NFT_MSG_DELRULE,
                                sequence: u32::from_ne_bytes(
                                    request.bytes[8..12].try_into().unwrap(),
                                ),
                                flags: 0,
                                echo: request.flags() & NLM_F_ECHO != 0,
                            });
                        }
                        Ok(())
                    })
                    .err(),
                NFT_MSG_DELCHAIN => chain_attrs(&request)
                    .and_then(|attrs| {
                        let (table, chain) = transaction.del_chain(
                            family,
                            attrs.table.ok_or(SystemError::EINVAL)?,
                            attrs.name,
                            attrs.handle,
                            request.flags() & 0x100 != 0,
                        )?;
                        notifications
                            .try_reserve(chain.rules().len() + 1)
                            .map_err(|_| SystemError::ENOMEM)?;
                        for rule in chain.rules() {
                            notifications.push(NftNotification {
                                object: NotificationObject::Rule(
                                    table.clone(),
                                    chain.clone(),
                                    rule.clone(),
                                ),
                                kind: NFT_MSG_DELRULE,
                                sequence: u32::from_ne_bytes(
                                    request.bytes[8..12].try_into().unwrap(),
                                ),
                                flags: 0,
                                echo: request.flags() & NLM_F_ECHO != 0,
                            });
                        }
                        notifications.push(NftNotification {
                            object: NotificationObject::Chain(table, chain),
                            kind: NFT_MSG_DELCHAIN,
                            sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
                            flags: 0,
                            echo: request.flags() & NLM_F_ECHO != 0,
                        });
                        Ok(())
                    })
                    .err(),
                NFT_MSG_DELTABLE => table_attrs(&request)
                    .and_then(|attrs| {
                        let removed = transaction.del_table(
                            family,
                            attrs.name,
                            attrs.handle,
                            request.flags() & 0x100 != 0,
                        )?;
                        notifications
                            .try_reserve(removed.len())
                            .map_err(|_| SystemError::ENOMEM)?;
                        for table in removed {
                            notifications.push(NftNotification {
                                object: NotificationObject::DeletedTableTree(table),
                                kind: NFT_MSG_DELTABLE,
                                sequence: u32::from_ne_bytes(
                                    request.bytes[8..12].try_into().unwrap(),
                                ),
                                flags: 0,
                                echo: request.flags() & NLM_F_ECHO != 0,
                            });
                        }
                        Ok(())
                    })
                    .err(),
                NFT_MSG_NEWSET => set_attrs(&request)
                    .and_then(|attrs| {
                        if !matches!(family, 1 | 2 | 10) || attrs.handle.is_some() {
                            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                        }
                        let created = transaction.new_set(NewSetSpec {
                            family,
                            table_name: attrs.table.ok_or(SystemError::EINVAL)?,
                            name: attrs.name.ok_or(SystemError::EINVAL)?,
                            key_type: attrs.key_type.ok_or(SystemError::EINVAL)?,
                            key_len: attrs.key_len.ok_or(SystemError::EINVAL)?,
                            data_type: attrs.data_type,
                            data_len: attrs.data_len,
                            size: attrs.size,
                            userdata: attrs.userdata.unwrap_or(&[]),
                            flags: attrs.flags,
                            id: attrs.id,
                            exclusive: request.flags() & 0x200 != 0,
                        })?;
                        if let Some((table, set)) = created {
                            notifications
                                .try_reserve(1)
                                .map_err(|_| SystemError::ENOMEM)?;
                            notifications.push(NftNotification {
                                object: NotificationObject::Set(table, set),
                                kind: NFT_MSG_NEWSET,
                                sequence: u32::from_ne_bytes(
                                    request.bytes[8..12].try_into().unwrap(),
                                ),
                                flags: request.flags() & (0x400 | 0x200),
                                echo: request.flags() & NLM_F_ECHO != 0,
                            });
                        }
                        Ok(())
                    })
                    .err(),
                NFT_MSG_DELSET => set_attrs(&request)
                    .and_then(|attrs| {
                        let (table, set) = transaction.del_set(
                            family,
                            attrs.table.ok_or(SystemError::EINVAL)?,
                            attrs.name.ok_or(SystemError::EINVAL)?,
                        )?;
                        notifications
                            .try_reserve(1)
                            .map_err(|_| SystemError::ENOMEM)?;
                        notifications.push(NftNotification {
                            object: NotificationObject::Set(table, set),
                            kind: NFT_MSG_DELSET,
                            sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
                            flags: 0,
                            echo: request.flags() & NLM_F_ECHO != 0,
                        });
                        Ok(())
                    })
                    .err(),
                NFT_MSG_NEWSETELEM | NFT_MSG_DELSETELEM => set_elem_attrs(&request)
                    .and_then(|attrs| {
                        if attrs.elements.is_empty() {
                            return Err(SystemError::EINVAL);
                        }
                        let table_name = attrs.table.ok_or(SystemError::EINVAL)?;
                        let name = transaction.resolve_set_name(
                            family,
                            table_name,
                            attrs.set,
                            attrs.set_id,
                        )?;
                        let add = request.kind() == NFT_MSG_NEWSETELEM;
                        let sequence = u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap());
                        let prior_events = if add {
                            Vec::new()
                        } else {
                            let (table, set) =
                                transaction.set_for_notification(family, table_name, &name)?;
                            set_element_events(
                                &table,
                                &set,
                                &attrs.elements,
                                sequence,
                                port,
                                request.kind(),
                            )?
                        };
                        let (table, set) = transaction.update_set_elements(
                            family,
                            table_name,
                            &name,
                            &attrs.elements,
                            add,
                        )?;
                        let events = if add {
                            set_element_events(
                                &table,
                                &set,
                                &attrs.elements,
                                sequence,
                                port,
                                request.kind(),
                            )?
                        } else {
                            prior_events
                        };
                        notifications
                            .try_reserve(events.len())
                            .map_err(|_| SystemError::ENOMEM)?;
                        for message in events {
                            notifications.push(NftNotification {
                                object: NotificationObject::SetElement(message),
                                kind: request.kind(),
                                sequence,
                                flags: 0,
                                echo: request.flags() & NLM_F_ECHO != 0,
                            });
                        }
                        Ok(())
                    })
                    .err(),
                _ => Some(SystemError::EINVAL),
            };
            failed |= error.is_some();
            if error.is_some() || request.flags() & 4 != 0 {
                replies.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                replies.push((request, error));
            }
            let advance = ((request.bytes.len() + 3) & !3).min(remaining.len());
            remaining = &remaining[advance..];
        }
        if !end_seen && remaining.len() >= HEADER_LEN {
            framing_error = true;
        }
        if !failed && end_seen {
            match transaction.commit(&netns, |generation| {
                let listeners =
                    NetlinkNetfilterProtocol::has_group_listeners(NFNLGRP_NFTABLES, netns.clone());
                let snapshot = netns.nftables().snapshot();
                for event in &notifications {
                    if !listeners && !event.echo {
                        continue;
                    }
                    if let NotificationObject::DeletedTableTree(table) = &event.object {
                        for chain in &table.chains {
                            for rule in chain.rules() {
                                Self::send_notification_result(
                                    rule_message(
                                        (table, chain),
                                        (rule, None),
                                        generation,
                                        event.sequence,
                                        port,
                                        NFT_MSG_DELRULE,
                                        0,
                                    ),
                                    event.echo,
                                    port,
                                    &netns,
                                );
                            }
                            Self::send_notification_result(
                                chain_message(
                                    table,
                                    chain,
                                    generation,
                                    event.sequence,
                                    port,
                                    NFT_MSG_DELCHAIN,
                                    0,
                                ),
                                event.echo,
                                port,
                                &netns,
                            );
                        }
                        for set in &table.sets {
                            Self::send_notification_result(
                                set_message(
                                    table,
                                    set,
                                    generation,
                                    event.sequence,
                                    port,
                                    NFT_MSG_DELSET,
                                    0,
                                ),
                                event.echo,
                                port,
                                &netns,
                            );
                        }
                        Self::send_notification_result(
                            table_message(
                                table,
                                generation,
                                event.sequence,
                                port,
                                NFT_MSG_DELTABLE,
                                0,
                                Some(0),
                            ),
                            event.echo,
                            port,
                            &netns,
                        );
                        continue;
                    }
                    let message = match &event.object {
                        NotificationObject::Table(table) => {
                            // Resolve creations against the published snapshot: a
                            // later operation in this batch may have changed USE.
                            let current = snapshot.tables.iter().find(|candidate| {
                                candidate.family == table.family && candidate.handle == table.handle
                            });
                            table_message(
                                current.unwrap_or(table),
                                generation,
                                event.sequence,
                                port,
                                event.kind,
                                event.flags,
                                if event.kind == NFT_MSG_DELTABLE {
                                    Some(0)
                                } else {
                                    None
                                },
                            )
                        }
                        NotificationObject::Chain(table, chain) => {
                            let current = snapshot
                                .tables
                                .iter()
                                .find(|candidate| {
                                    candidate.family == table.family
                                        && candidate.handle == table.handle
                                })
                                .and_then(|current_table| {
                                    current_table
                                        .chains
                                        .iter()
                                        .find(|current_chain| current_chain.handle == chain.handle)
                                        .map(|current_chain| (current_table, current_chain))
                                });
                            let (table, chain) = current.map_or((table, chain), |(t, c)| (t, c));
                            chain_message(
                                table,
                                chain,
                                generation,
                                event.sequence,
                                port,
                                event.kind,
                                event.flags,
                            )
                        }
                        NotificationObject::Rule(table, chain, rule) => {
                            let current = snapshot
                                .tables
                                .iter()
                                .find(|candidate| {
                                    candidate.family == table.family
                                        && candidate.handle == table.handle
                                })
                                .and_then(|current_table| {
                                    current_table
                                        .chains
                                        .iter()
                                        .find(|current_chain| current_chain.handle == chain.handle)
                                        .and_then(|current_chain| {
                                            current_chain
                                                .rules()
                                                .iter()
                                                .find(|current_rule| {
                                                    current_rule.handle == rule.handle
                                                })
                                                .map(|current_rule| {
                                                    (current_table, current_chain, current_rule)
                                                })
                                        })
                                });
                            let (table, chain, rule) =
                                current.map_or((table, chain, rule), |(t, c, r)| (t, c, r));
                            // Linux reports the predecessor only for an
                            // insertion in the middle of a chain. Appending
                            // at the tail is represented by NLM_F_APPEND.
                            let position = if event.kind == NFT_MSG_NEWRULE {
                                chain
                                    .rules()
                                    .iter()
                                    .position(|candidate| candidate.handle == rule.handle)
                                    .and_then(|index| {
                                        if index == 0 || index + 1 == chain.rules().len() {
                                            None
                                        } else {
                                            chain.rules().get(index - 1).map(|prev| prev.handle)
                                        }
                                    })
                            } else {
                                None
                            };
                            rule_message(
                                (table, chain),
                                (rule, position),
                                generation,
                                event.sequence,
                                port,
                                event.kind,
                                event.flags,
                            )
                        }
                        NotificationObject::Set(table, set) => {
                            let current = snapshot
                                .tables
                                .iter()
                                .find(|candidate| {
                                    candidate.family == table.family
                                        && candidate.handle == table.handle
                                })
                                .and_then(|candidate| {
                                    candidate
                                        .sets
                                        .iter()
                                        .find(|item| item.handle == set.handle)
                                        .map(|item| (candidate, item))
                                });
                            let (table, set) =
                                current.map_or((table, set), |(table, set)| (table, set));
                            set_message(
                                table,
                                set,
                                generation,
                                event.sequence,
                                port,
                                event.kind,
                                event.flags,
                            )
                        }
                        NotificationObject::SetElement(message) => {
                            let mut bytes = message.0.as_ref().clone();
                            bytes[HEADER_LEN + 2..HEADER_LEN + NFGEN_LEN]
                                .copy_from_slice(&(generation as u16).to_be_bytes());
                            NetfilterMessage::new(bytes)
                        }
                        NotificationObject::DeletedTableTree(_) => unreachable!(),
                    };
                    Self::send_notification_result(message, event.echo, port, &netns);
                }
                if listeners || begin.flags() & NLM_F_ECHO != 0 {
                    if let Ok(message) = Self::generation_message(begin, port, &netns) {
                        Self::notify(message, begin.flags() & NLM_F_ECHO != 0, port, &netns);
                    } else {
                        NetlinkNetfilterProtocol::report_group_overrun(
                            NFNLGRP_NFTABLES,
                            port,
                            netns.clone(),
                        );
                    }
                }
            }) {
                Ok(_) => (),
                Err(error) => Self::ack(begin, Some(error), port, netns.clone())?,
            }
        } else {
            drop(transaction);
        }
        if framing_error {
            // Linux resets the deferred ACK list only for malformed framing.
            // Semantic errors and commit failures retain earlier ACKs even
            // though the transaction itself is rolled back.
            replies.clear();
        }
        for (request, error) in replies {
            Self::ack(&request, error, port, netns.clone())?;
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

    fn send_generation(
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        let message = Self::generation_message(request, port, netns)?;
        NetlinkNetfilterProtocol::unicast(port, message, netns.clone())
    }

    fn send_notification_result(
        result: Result<NetfilterMessage, SystemError>,
        echo: bool,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) {
        if let Ok(message) = result {
            Self::notify(message, echo, port, netns);
        } else {
            NetlinkNetfilterProtocol::report_group_overrun(NFNLGRP_NFTABLES, port, netns.clone());
        }
    }

    fn notify(message: NetfilterMessage, echo: bool, port: u32, netns: &Arc<NetNamespace>) {
        if echo {
            let _ = NetlinkNetfilterProtocol::unicast(port, message.clone(), netns.clone());
        }
        NetlinkNetfilterProtocol::notify_group(
            NFNLGRP_NFTABLES,
            if echo { port } else { 0 },
            message,
            netns.clone(),
        );
    }

    fn generation_message(
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<NetfilterMessage, SystemError> {
        let generation = netns.nftables().generation();
        let task = ProcessManager::current_pcb();
        let mut comm = [0u8; 16];
        let comm_len = {
            let basic = task.basic();
            let name = basic.name().as_bytes();
            let len = name.len().min(comm.len() - 1);
            comm[..len].copy_from_slice(&name[..len]);
            len + 1
        };
        let name_attr_len = (4 + comm_len + 3) & !3;
        let len = HEADER_LEN + NFGEN_LEN + 8 + 8 + name_attr_len;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.extend_from_slice(&(len as u32).to_ne_bytes());
        bytes.extend_from_slice(&NFT_MSG_NEWGEN.to_ne_bytes());
        bytes.extend_from_slice(&0u16.to_ne_bytes());
        bytes.extend_from_slice(&request.bytes[8..12]);
        bytes.extend_from_slice(&port.to_ne_bytes());
        bytes.push(0); // AF_UNSPEC
        bytes.push(0); // NFNETLINK_V0
        bytes.extend_from_slice(&(generation as u16).to_be_bytes());
        bytes.extend_from_slice(&8u16.to_ne_bytes());
        bytes.extend_from_slice(&1u16.to_ne_bytes()); // NFTA_GEN_ID
        bytes.extend_from_slice(&generation.to_be_bytes());
        bytes.extend_from_slice(&8u16.to_ne_bytes());
        bytes.extend_from_slice(&2u16.to_ne_bytes()); // NFTA_GEN_PROC_PID
        bytes.extend_from_slice(&(ProcessManager::current_pid().data() as u32).to_be_bytes());
        bytes.extend_from_slice(&((4 + comm_len) as u16).to_ne_bytes());
        bytes.extend_from_slice(&3u16.to_ne_bytes()); // NFTA_GEN_PROC_NAME
        bytes.extend_from_slice(&comm[..comm_len]);
        bytes.resize(len, 0);
        debug_assert_eq!(bytes.len(), len);
        NetfilterMessage::new(bytes)
    }
}

fn batch_error(request: &Request<'_>) -> SystemError {
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
    let raw = [request.bytes[18], request.bytes[19]];
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
