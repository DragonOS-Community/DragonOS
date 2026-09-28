//! nf_tables object UAPI. Dumps retain a cursor, not a ruleset snapshot.

use super::{
    NetfilterMessage, Request, HEADER_LEN, NFGEN_LEN, NFT_MSG_GETFLOWTABLE, NFT_MSG_GETSET,
    NFT_MSG_NEWCHAIN, NFT_MSG_NEWSET, NFT_MSG_NEWSETELEM,
};
use crate::{
    libs::mutex::Mutex,
    net::{
        nftables::{
            NftBitwiseOperation, NftByteorderOp, NftChain, NftChainType, NftCmpOp, NftExpression,
            NftExpressionInput, NftMetaKey, NftPayloadBase, NftRangeOp, NftRule, NftRuleInput,
            NftRuleVerdict, NftSet, NftSetElementInput, NftTable, NftVerdict, RulesetSnapshot,
        },
        socket::netlink::{
            receiver::MessageQueue,
            table::{NetlinkNetfilterProtocol, SupportedNetlinkProtocol},
        },
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

pub(super) const NFT_MSG_NEWTABLE: u16 = 10 << 8;
const ATTR_NAME: u16 = 1;
const ATTR_FLAGS: u16 = 2;
const ATTR_USE: u16 = 3;
const ATTR_HANDLE: u16 = 4;
const ATTR_PAD: u16 = 5;
const ATTR_USERDATA: u16 = 6;
const CHAIN_TABLE: u16 = 1;
const CHAIN_HANDLE: u16 = 2;
const CHAIN_NAME: u16 = 3;
const CHAIN_HOOK: u16 = 4;
const CHAIN_POLICY: u16 = 5;
const CHAIN_USE: u16 = 6;
const CHAIN_TYPE: u16 = 7;
const CHAIN_PAD: u16 = 9;
const CHAIN_FLAGS: u16 = 10;
const HOOK_NUM: u16 = 1;
const HOOK_PRIORITY: u16 = 2;
const RULE_TABLE: u16 = 1;
const RULE_CHAIN: u16 = 2;
const RULE_HANDLE: u16 = 3;
const RULE_EXPRESSIONS: u16 = 4;
const RULE_COMPAT: u16 = 5;
const RULE_POSITION: u16 = 6;
const RULE_PAD: u16 = 8;
const LIST_ELEM: u16 = 1;
const EXPR_NAME: u16 = 1;
const EXPR_DATA: u16 = 2;
const IMMEDIATE_DREG: u16 = 1;
const IMMEDIATE_DATA: u16 = 2;
const DATA_VERDICT: u16 = 2;
const DATA_VALUE: u16 = 1;
const VERDICT_CODE: u16 = 1;
const VERDICT_CHAIN: u16 = 2;
const PAYLOAD_DREG: u16 = 1;
const PAYLOAD_BASE: u16 = 2;
const PAYLOAD_OFFSET: u16 = 3;
const PAYLOAD_LEN: u16 = 4;
const META_DREG: u16 = 1;
const META_KEY: u16 = 2;
const META_SREG: u16 = 3;
const FIB_DREG: u16 = 1;
const FIB_RESULT: u16 = 2;
const FIB_FLAGS: u16 = 3;
const CMP_SREG: u16 = 1;
const CMP_OP: u16 = 2;
const CMP_DATA: u16 = 3;
const BYTEORDER_SREG: u16 = 1;
const BYTEORDER_DREG: u16 = 2;
const BYTEORDER_OP: u16 = 3;
const BYTEORDER_LEN: u16 = 4;
const BYTEORDER_SIZE: u16 = 5;
const RANGE_SREG: u16 = 1;
const RANGE_OP: u16 = 2;
const RANGE_FROM_DATA: u16 = 3;
const RANGE_TO_DATA: u16 = 4;
const BITWISE_SREG: u16 = 1;
const BITWISE_DREG: u16 = 2;
const BITWISE_LEN: u16 = 3;
const BITWISE_MASK: u16 = 4;
const BITWISE_XOR: u16 = 5;
const BITWISE_OP: u16 = 6;
const BITWISE_DATA: u16 = 7;
const COUNTER_BYTES: u16 = 1;
const COUNTER_PACKETS: u16 = 2;
const COUNTER_PAD: u16 = 3;
const MATCH_NAME: u16 = 1;
const MATCH_REV: u16 = 2;
const MATCH_INFO: u16 = 3;
// nf_tables_compat.h gives match and target expressions the same attribute
// numbers; their names and payload layouts are validated separately.
const CT_DREG: u16 = 1;
const CT_KEY: u16 = 2;
const CT_DIRECTION: u16 = 3;
const CT_SREG: u16 = 4;
const NAT_TYPE: u16 = 1;
const NAT_FAMILY: u16 = 2;
const NAT_REG_ADDR_MIN: u16 = 3;
const NAT_REG_ADDR_MAX: u16 = 4;
const NAT_REG_PROTO_MIN: u16 = 5;
const NAT_REG_PROTO_MAX: u16 = 6;
const NAT_FLAGS: u16 = 7;
const MASQ_FLAGS: u16 = 1;
const MASQ_REG_PROTO_MIN: u16 = 2;
const MASQ_REG_PROTO_MAX: u16 = 3;
const REDIR_REG_PROTO_MIN: u16 = 1;
const REDIR_REG_PROTO_MAX: u16 = 2;
const REDIR_FLAGS: u16 = 3;
const SET_TABLE: u16 = 1;
const SET_NAME: u16 = 2;
const SET_FLAGS: u16 = 3;
const SET_KEY_TYPE: u16 = 4;
const SET_KEY_LEN: u16 = 5;
const SET_DATA_TYPE: u16 = 6;
const SET_DATA_LEN: u16 = 7;
const SET_POLICY: u16 = 8;
const SET_DESC: u16 = 9;
const SET_DESC_SIZE: u16 = 1;
const SET_ID: u16 = 10;
const SET_USERDATA: u16 = 13;
const SET_HANDLE: u16 = 16;
const SET_ELEM_TABLE: u16 = 1;
const SET_ELEM_SET: u16 = 2;
const SET_ELEM_ELEMENTS: u16 = 3;
const SET_ELEM_SET_ID: u16 = 4;
const SET_ELEMENT_KEY: u16 = 1;
const SET_ELEMENT_DATA: u16 = 2;
const SET_ELEMENT_FLAGS: u16 = 3;
const SET_ELEMENT_KEY_END: u16 = 10;
const LOOKUP_SET: u16 = 1;
const LOOKUP_SREG: u16 = 2;
const LOOKUP_DREG: u16 = 3;
const LOOKUP_SET_ID: u16 = 4;
const LOOKUP_FLAGS: u16 = 5;
const RULE_COMPAT_PROTO: u16 = 1;
const RULE_COMPAT_FLAGS: u16 = 2;
const COMPAT_NAME: u16 = 1;
const COMPAT_REV: u16 = 2;
const COMPAT_TYPE: u16 = 3;
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_MULTI: u16 = 2;
const NLM_F_DUMP_INTR: u16 = 0x10;

/// Validated netlink attributes, including the wire alignment shared by
/// table, chain and rule messages. Policy-specific length checks stay with
/// each object decoder.
pub(super) struct Nla<'a> {
    pub(super) kind: u16,
    pub(super) value: &'a [u8],
}

pub(super) struct NlaIter<'a> {
    remaining: &'a [u8],
}

impl<'a> NlaIter<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }
}

impl<'a> Iterator for NlaIter<'a> {
    type Item = Result<Nla<'a>, SystemError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        if self.remaining.len() < 4 {
            self.remaining = &[];
            return Some(Err(SystemError::EINVAL));
        }
        let len = u16::from_ne_bytes([self.remaining[0], self.remaining[1]]) as usize;
        if len < 4 || len > self.remaining.len() {
            self.remaining = &[];
            return Some(Err(SystemError::EINVAL));
        }
        let kind = u16::from_ne_bytes([self.remaining[2], self.remaining[3]]) & 0x3fff;
        let value = &self.remaining[4..len];
        self.remaining = &self.remaining[((len + 3) & !3).min(self.remaining.len())..];
        Some(Ok(Nla { kind, value }))
    }
}

#[derive(Debug)]
pub struct NftSocketState {
    dump: Mutex<Option<DumpSession>>,
}

impl Default for NftSocketState {
    fn default() -> Self {
        Self {
            dump: Mutex::new(None),
        }
    }
}

#[derive(Debug)]
struct TableDumpSession {
    generation: u32,
    family: u8,
    sequence: u32,
    cursor: usize,
}

#[derive(Debug)]
struct ChainDumpSession {
    generation: u32,
    family: u8,
    sequence: u32,
    table_cursor: usize,
    chain_cursor: usize,
}

#[derive(Debug)]
struct RuleDumpSession {
    generation: u32,
    family: u8,
    sequence: u32,
    table_name: Option<Vec<u8>>,
    chain_name: Option<Vec<u8>>,
    table_cursor: usize,
    chain_cursor: usize,
    rule_cursor: usize,
}

#[derive(Debug)]
struct SetDumpSession {
    generation: u32,
    family: u8,
    sequence: u32,
    table_name: Option<Vec<u8>>,
    set_name: Option<Vec<u8>>,
    table_cursor: usize,
    set_cursor: usize,
}

#[derive(Debug)]
struct SetElementDumpSession {
    generation: u32,
    family: u8,
    sequence: u32,
    table_name: Vec<u8>,
    set_name: Vec<u8>,
    cursor: usize,
}

#[derive(Debug)]
enum DumpSession {
    Tables(TableDumpSession),
    Chains(ChainDumpSession),
    Rules(RuleDumpSession),
    Sets(SetDumpSession),
    SetElements(SetElementDumpSession),
    /// These object classes have no committed instances until their creation
    /// and packet execution paths are implemented together. Their empty dump
    /// is still a real read of the namespace ruleset, not a successful write.
    Empty {
        sequence: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GetTableResult {
    Replied,
    DumpStarted,
}

#[derive(Default)]
pub(super) struct TableAttrs<'a> {
    pub(super) name: Option<&'a [u8]>,
    pub(super) flags: u32,
    pub(super) handle: Option<u64>,
    pub(super) userdata: &'a [u8],
}

#[derive(Default)]
pub(super) struct ChainAttrs<'a> {
    pub(super) table: Option<&'a [u8]>,
    pub(super) name: Option<&'a [u8]>,
    pub(super) handle: Option<u64>,
    pub(super) hook: Option<(u32, i32)>,
    pub(super) policy: Option<NftVerdict>,
    pub(super) chain_type: Option<&'a [u8]>,
    pub(super) flags: u32,
}

#[derive(Default)]
pub(super) struct RuleAttrs<'a> {
    pub(super) table: Option<&'a [u8]>,
    pub(super) chain: Option<&'a [u8]>,
    pub(super) handle: Option<u64>,
    pub(super) position: Option<u64>,
    pub(super) expressions: Option<Vec<NftExpressionInput<'a>>>,
    pub(super) compat: Option<&'a [u8]>,
}

#[derive(Default)]
pub(super) struct SetAttrs<'a> {
    pub(super) table: Option<&'a [u8]>,
    pub(super) name: Option<&'a [u8]>,
    pub(super) flags: u32,
    pub(super) key_type: Option<u32>,
    pub(super) key_len: Option<usize>,
    pub(super) data_type: Option<u32>,
    pub(super) data_len: Option<usize>,
    pub(super) size: Option<usize>,
    pub(super) id: Option<u32>,
    pub(super) handle: Option<u64>,
    pub(super) userdata: Option<&'a [u8]>,
}

#[derive(Default)]
pub(super) struct SetElemAttrs<'a> {
    pub(super) table: Option<&'a [u8]>,
    pub(super) set: Option<&'a [u8]>,
    pub(super) set_id: Option<u32>,
    pub(super) elements: Vec<NftSetElementInput<'a>>,
}

pub(super) fn xt_tcp_compatible(compat: Option<&[u8]>) -> Result<(), SystemError> {
    let (proto, flags) = xt_compat_fields(compat)?;
    if flags != 0 || proto != 6 {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

pub(super) fn xt_addrtype_compatible(compat: Option<&[u8]>) -> Result<(), SystemError> {
    // nft_compat defaults these validation-only fields to zero when the
    // optional rule attribute is absent. addrtype itself is not L4-specific.
    let Some(compat) = compat else {
        return Ok(());
    };
    let (proto, flags) = xt_compat_fields(Some(compat))?;
    if proto > u16::MAX as u32 || flags & !0x2 != 0 {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

pub(super) fn xt_conntrack_compatible(compat: Option<&[u8]>) -> Result<(), SystemError> {
    let Some(compat) = compat else {
        return Ok(());
    };
    let (proto, flags) = xt_compat_fields(Some(compat))?;
    if proto != 0 || flags != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(())
}

fn xt_compat_fields(compat: Option<&[u8]>) -> Result<(u32, u32), SystemError> {
    let mut proto = None;
    let mut flags = None;
    for nla in NlaIter::new(compat.ok_or(SystemError::EINVAL)?) {
        let nla = nla?;
        match nla.kind {
            RULE_COMPAT_PROTO if proto.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RULE_COMPAT_PROTO => {}
            RULE_COMPAT_FLAGS if flags.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RULE_COMPAT_FLAGS => {}
            _ => {}
        }
    }
    Ok((
        proto.ok_or(SystemError::EINVAL)?,
        flags.ok_or(SystemError::EINVAL)?,
    ))
}

fn xt_extension(bytes: &[u8]) -> Result<(&[u8], u32, &[u8]), SystemError> {
    let mut name = None;
    let mut revision = None;
    let mut info = None;
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            MATCH_NAME if name.replace(compat_name(nla.value, 29)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            MATCH_NAME => {}
            MATCH_REV if revision.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            MATCH_REV => {}
            MATCH_INFO if info.replace(nla.value).is_some() => return Err(SystemError::EINVAL),
            MATCH_INFO => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    let name = name.ok_or(SystemError::EINVAL)?;
    let revision = revision.ok_or(SystemError::EINVAL)?;
    let info = info.ok_or(SystemError::EINVAL)?;
    if revision > u8::MAX as u32 {
        return Err(SystemError::ERANGE);
    }
    Ok((name, revision, info))
}

fn xt_match(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (name, revision, info) = xt_extension(bytes)?;
    match (name, revision) {
        (b"tcp", 0) => Ok(NftExpressionInput::XtTcp(info)),
        (b"addrtype", 1) => Ok(NftExpressionInput::XtAddrtype(info)),
        (b"conntrack", 1..=3) => Ok(NftExpressionInput::XtConntrack { revision, info }),
        _ => Err(SystemError::ENOENT),
    }
}

fn xt_target(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (name, revision, info) = xt_extension(bytes)?;
    Ok(NftExpressionInput::XtTarget {
        name,
        revision,
        info,
    })
}

fn ct_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut dreg, mut key, mut direction) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            CT_DREG if dreg.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            CT_DREG => {}
            CT_KEY if key.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            CT_KEY => {}
            CT_DIRECTION
                if direction
                    .replace(*nla.value.first().ok_or(SystemError::ERANGE)?)
                    .is_some() =>
            {
                return Err(SystemError::EINVAL);
            }
            CT_DIRECTION => {}
            // The write side needs a separate typed execution path.  It must
            // not be mistaken for a read and acknowledged as a no-op.
            CT_SREG => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(NftExpressionInput::Ct {
        key: key.ok_or(SystemError::EINVAL)?,
        dreg: dreg.ok_or(SystemError::EINVAL)?,
        direction,
    })
}

fn nat_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut nat_type, mut family, mut addr_min_reg, mut addr_max_reg) = (None, None, None, None);
    let (mut proto_min_reg, mut proto_max_reg, mut flags) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        let field = match nla.kind {
            NAT_TYPE => &mut nat_type,
            NAT_FAMILY => &mut family,
            NAT_REG_ADDR_MIN => &mut addr_min_reg,
            NAT_REG_ADDR_MAX => &mut addr_max_reg,
            NAT_REG_PROTO_MIN => &mut proto_min_reg,
            NAT_REG_PROTO_MAX => &mut proto_max_reg,
            NAT_FLAGS => &mut flags,
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        if field.replace(be_u32(nla.value)?).is_some() {
            return Err(SystemError::EINVAL);
        }
    }
    if addr_min_reg.is_none() && proto_min_reg.is_none() {
        return Err(SystemError::EINVAL);
    }
    if flags.is_some_and(|value| value & !0x7f != 0) {
        return Err(SystemError::EINVAL);
    }
    // Linux's nft_nat_init() only consults a MAX attribute inside the
    // corresponding MIN branch.  An orphan MAX is accepted but has no effect.
    let addr_max_reg = addr_max_reg.filter(|_| addr_min_reg.is_some());
    let proto_max_reg = proto_max_reg.filter(|_| proto_min_reg.is_some());
    Ok(NftExpressionInput::Nat {
        nat_type: nat_type.ok_or(SystemError::EINVAL)?,
        family: family.ok_or(SystemError::EINVAL)?,
        addr_min_reg,
        addr_max_reg,
        proto_min_reg,
        proto_max_reg,
        flags: flags.unwrap_or(0),
    })
}

fn masq_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut flags, mut proto_min_reg, mut proto_max_reg) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        let field = match nla.kind {
            MASQ_FLAGS => &mut flags,
            MASQ_REG_PROTO_MIN => &mut proto_min_reg,
            MASQ_REG_PROTO_MAX => &mut proto_max_reg,
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        if field.replace(be_u32(nla.value)?).is_some() {
            return Err(SystemError::EINVAL);
        }
    }
    // nft_masq_init() likewise ignores MAX when MIN was omitted.
    let proto_max_reg = proto_max_reg.filter(|_| proto_min_reg.is_some());
    if flags.is_some_and(|value| value & !0x7f != 0) {
        return Err(SystemError::EINVAL);
    }
    Ok(NftExpressionInput::Masq {
        flags: flags.unwrap_or(0),
        proto_min_reg,
        proto_max_reg,
    })
}

fn redirect_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut flags, mut proto_min_reg, mut proto_max_reg) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        let field = match nla.kind {
            REDIR_REG_PROTO_MIN => &mut proto_min_reg,
            REDIR_REG_PROTO_MAX => &mut proto_max_reg,
            REDIR_FLAGS => &mut flags,
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        if field.replace(be_u32(nla.value)?).is_some() {
            return Err(SystemError::EINVAL);
        }
    }
    if flags.is_some_and(|value| value & !0x7f != 0) {
        return Err(SystemError::EINVAL);
    }
    // Linux nft_redir_init() ignores MAX without MIN.
    Ok(NftExpressionInput::Redirect {
        flags,
        proto_min_reg,
        proto_max_reg: proto_max_reg.filter(|_| proto_min_reg.is_some()),
    })
}

fn lookup_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut set, mut sreg, mut dreg, mut flags, mut set_id) = (None, None, None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            LOOKUP_SET if set.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            LOOKUP_SET => {}
            LOOKUP_SREG if sreg.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            LOOKUP_SREG => {}
            LOOKUP_FLAGS if flags.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            LOOKUP_FLAGS => {}
            LOOKUP_SET_ID if set_id.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            LOOKUP_SET_ID => {}
            LOOKUP_DREG if dreg.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            LOOKUP_DREG => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    let flags = flags.unwrap_or(0);
    if flags & !1 != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if set.is_none() && set_id.is_none() {
        return Err(SystemError::EINVAL);
    }
    Ok(NftExpressionInput::Lookup {
        set,
        set_id,
        sreg: sreg.ok_or(SystemError::EINVAL)?,
        dreg,
        invert: flags & 1 != 0,
    })
}

fn be_u32(value: &[u8]) -> Result<u32, SystemError> {
    let bytes: [u8; 4] = value
        .get(..4)
        .ok_or(SystemError::ERANGE)?
        .try_into()
        .unwrap();
    Ok(u32::from_be_bytes(bytes))
}

fn be_u64(value: &[u8]) -> Result<u64, SystemError> {
    let bytes: [u8; 8] = value
        .get(..8)
        .ok_or(SystemError::ERANGE)?
        .try_into()
        .unwrap();
    Ok(u64::from_be_bytes(bytes))
}

fn immediate_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let mut register = None;
    let mut value = None;
    let mut verdict = false;
    let mut seen_data = false;
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            IMMEDIATE_DREG if register.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            IMMEDIATE_DREG => {}
            IMMEDIATE_DATA => {
                if seen_data {
                    return Err(SystemError::EINVAL);
                }
                seen_data = true;
                let mut nested_seen = false;
                for nested in NlaIter::new(nla.value) {
                    let nested = nested?;
                    if nested_seen {
                        return Err(SystemError::EINVAL);
                    }
                    nested_seen = true;
                    match nested.kind {
                        DATA_VALUE => value = Some(nested.value),
                        DATA_VERDICT => verdict = true,
                        _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                    }
                }
                if !nested_seen {
                    return Err(SystemError::EINVAL);
                }
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    if let Some(data) = value {
        return Ok(NftExpressionInput::ImmediateData {
            dreg: register.ok_or(SystemError::EINVAL)?,
            data,
        });
    }
    if verdict {
        return Ok(NftExpressionInput::Immediate(immediate_verdict(bytes)?));
    }
    Err(SystemError::EINVAL)
}

fn immediate_verdict(bytes: &[u8]) -> Result<NftRuleInput<'_>, SystemError> {
    let mut register = None;
    let mut parsed = None;
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            IMMEDIATE_DREG if register.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            IMMEDIATE_DREG => {}
            IMMEDIATE_DATA => {
                if parsed.is_some() {
                    return Err(SystemError::EINVAL);
                }
                let mut verdict_data = None;
                for data in NlaIter::new(nla.value) {
                    let data = data?;
                    if data.kind != DATA_VERDICT || verdict_data.replace(data.value).is_some() {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                }
                parsed = Some(parse_verdict(verdict_data.ok_or(SystemError::EINVAL)?)?);
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    if register != Some(0) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    parsed.ok_or(SystemError::EINVAL)
}

fn parse_verdict(bytes: &[u8]) -> Result<NftRuleInput<'_>, SystemError> {
    let mut code = None;
    let mut target = None;
    for verdict in NlaIter::new(bytes) {
        let verdict = verdict?;
        match verdict.kind {
            VERDICT_CODE if code.replace(be_u32(verdict.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            VERDICT_CODE => {}
            VERDICT_CHAIN if target.replace(nla_name(verdict.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            VERDICT_CHAIN => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    match (code.ok_or(SystemError::EINVAL)?, target) {
        (0, None) => Ok(NftRuleInput::Drop),
        (1, None) => Ok(NftRuleInput::Accept),
        (code, Some(target)) if code == (-3i32) as u32 => Ok(NftRuleInput::Jump(target)),
        (code, Some(target)) if code == (-4i32) as u32 => Ok(NftRuleInput::Goto(target)),
        (code, None) if code == (-5i32) as u32 => Ok(NftRuleInput::Return),
        (code, None) if code == (-1i32) as u32 => Ok(NftRuleInput::Continue),
        _ => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
    }
}

fn payload_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut dreg, mut base, mut offset, mut len) = (None, None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        let field = match nla.kind {
            PAYLOAD_DREG => &mut dreg,
            PAYLOAD_BASE => &mut base,
            PAYLOAD_OFFSET => &mut offset,
            PAYLOAD_LEN => &mut len,
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        if field.replace(be_u32(nla.value)?).is_some() {
            return Err(SystemError::EINVAL);
        }
    }
    Ok(NftExpressionInput::Payload {
        dreg: dreg.ok_or(SystemError::EINVAL)?,
        base: base.ok_or(SystemError::EINVAL)?,
        offset: offset.ok_or(SystemError::EINVAL)?,
        len: len.ok_or(SystemError::EINVAL)?,
    })
}

fn meta_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut dreg, mut key, mut sreg) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        let field = match nla.kind {
            META_DREG => &mut dreg,
            META_KEY => &mut key,
            META_SREG => &mut sreg,
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        if field.replace(be_u32(nla.value)?).is_some() {
            return Err(SystemError::EINVAL);
        }
    }
    let key = key.ok_or(SystemError::EINVAL)?;
    match (dreg, sreg) {
        (Some(dreg), None) => Ok(NftExpressionInput::Meta { key, dreg }),
        (None, Some(sreg)) => Ok(NftExpressionInput::MetaSet { key, sreg }),
        _ => Err(SystemError::EINVAL),
    }
}

fn fib_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut dreg, mut result, mut flags) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        let field = match nla.kind {
            FIB_DREG => &mut dreg,
            FIB_RESULT => &mut result,
            FIB_FLAGS => &mut flags,
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        if field.replace(be_u32(nla.value)?).is_some() {
            return Err(SystemError::EINVAL);
        }
    }
    Ok(NftExpressionInput::Fib {
        dreg: dreg.ok_or(SystemError::EINVAL)?,
        result: result.ok_or(SystemError::EINVAL)?,
        flags: flags.ok_or(SystemError::EINVAL)?,
    })
}

fn compare_data(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut sreg, mut op, mut data) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            CMP_SREG => {
                if sreg.replace(be_u32(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            CMP_OP => {
                if op.replace(be_u32(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            CMP_DATA => {
                if data.is_some() {
                    return Err(SystemError::EINVAL);
                }
                data = Some(nested_value(nla.value)?);
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(NftExpressionInput::Cmp {
        sreg: sreg.ok_or(SystemError::EINVAL)?,
        op: op.ok_or(SystemError::EINVAL)?,
        data: data.ok_or(SystemError::EINVAL)?,
    })
}

fn byteorder_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut sreg, mut dreg, mut op, mut len, mut size) = (None, None, None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        let field = match nla.kind {
            BYTEORDER_SREG => &mut sreg,
            BYTEORDER_DREG => &mut dreg,
            BYTEORDER_OP => &mut op,
            BYTEORDER_LEN => &mut len,
            BYTEORDER_SIZE => &mut size,
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        if field.replace(be_u32(nla.value)?).is_some() {
            return Err(SystemError::EINVAL);
        }
    }
    Ok(NftExpressionInput::Byteorder {
        sreg: sreg.ok_or(SystemError::EINVAL)?,
        dreg: dreg.ok_or(SystemError::EINVAL)?,
        op: op.ok_or(SystemError::EINVAL)?,
        len: len.ok_or(SystemError::EINVAL)?,
        size: size.ok_or(SystemError::EINVAL)?,
    })
}

fn range_read(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut sreg, mut op, mut from, mut to) = (None, None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            RANGE_SREG if sreg.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RANGE_OP if op.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RANGE_FROM_DATA if from.replace(nested_value(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RANGE_TO_DATA if to.replace(nested_value(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RANGE_SREG | RANGE_OP | RANGE_FROM_DATA | RANGE_TO_DATA => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(NftExpressionInput::Range {
        sreg: sreg.ok_or(SystemError::EINVAL)?,
        op: op.ok_or(SystemError::EINVAL)?,
        from: from.ok_or(SystemError::EINVAL)?,
        to: to.ok_or(SystemError::EINVAL)?,
    })
}

fn nested_value(bytes: &[u8]) -> Result<&[u8], SystemError> {
    let mut nested = NlaIter::new(bytes);
    let value = nested.next().ok_or(SystemError::EINVAL)??;
    if value.kind != DATA_VALUE || nested.next().is_some() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(value.value)
}

fn bitwise_data(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut sreg, mut dreg, mut len, mut op) = (None, None, None, None);
    let (mut mask, mut xor, mut data) = (None, None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            BITWISE_SREG => {
                if sreg.replace(be_u32(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            BITWISE_DREG => {
                if dreg.replace(be_u32(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            BITWISE_LEN => {
                if len.replace(be_u32(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            BITWISE_OP => {
                if op.replace(be_u32(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            BITWISE_MASK => {
                if mask.replace(nested_value(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            BITWISE_XOR => {
                if xor.replace(nested_value(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            BITWISE_DATA => {
                if data.replace(nested_value(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    let op = op.unwrap_or(0);
    if op > u8::MAX as u32 {
        return Err(SystemError::ERANGE);
    }
    Ok(NftExpressionInput::Bitwise {
        sreg: sreg.ok_or(SystemError::EINVAL)?,
        dreg: dreg.ok_or(SystemError::EINVAL)?,
        len: len.ok_or(SystemError::EINVAL)?,
        op,
        mask,
        xor,
        data,
    })
}

fn counter_attrs(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let (mut packets, mut counted_bytes) = (None, None);
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            COUNTER_PACKETS => {
                if packets.replace(be_u64(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            COUNTER_BYTES => {
                if counted_bytes.replace(be_u64(nla.value)?).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            COUNTER_PAD => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(NftExpressionInput::Counter {
        packets: packets.unwrap_or(0),
        bytes: counted_bytes.unwrap_or(0),
    })
}

fn rule_expression(bytes: &[u8]) -> Result<NftExpressionInput<'_>, SystemError> {
    let mut name = None;
    let mut data = None;
    for nla in NlaIter::new(bytes) {
        let nla = nla?;
        match nla.kind {
            EXPR_NAME if name.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            EXPR_NAME => {}
            EXPR_DATA if data.replace(nla.value).is_some() => return Err(SystemError::EINVAL),
            EXPR_DATA => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    let data = data.ok_or(SystemError::EINVAL)?;
    match name.ok_or(SystemError::EINVAL)? {
        b"immediate" => immediate_read(data),
        b"payload" => payload_read(data),
        b"meta" => meta_read(data),
        b"fib" => fib_read(data),
        b"cmp" => compare_data(data),
        b"byteorder" => byteorder_read(data),
        b"range" => range_read(data),
        b"bitwise" => bitwise_data(data),
        b"counter" => counter_attrs(data),
        b"match" => xt_match(data),
        b"target" => xt_target(data),
        b"ct" => ct_read(data),
        b"nat" => nat_read(data),
        b"masq" => masq_read(data),
        b"redir" => redirect_read(data),
        b"lookup" => lookup_read(data),
        _ => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
    }
}

#[cfg(test)]
mod expression_decoder_tests {
    use super::*;

    #[test]
    fn immediate_data_value_is_distinct_from_terminal_verdict() {
        let mut nested = Vec::new();
        append_attr(&mut nested, DATA_VALUE, &[127, 0, 0, 1]).unwrap();
        let mut data = Vec::new();
        append_attr(&mut data, IMMEDIATE_DREG, &1u32.to_be_bytes()).unwrap();
        append_attr(&mut data, IMMEDIATE_DATA, &nested).unwrap();
        assert!(matches!(
            immediate_read(&data),
            Ok(NftExpressionInput::ImmediateData { dreg: 1, data })
                if data == [127, 0, 0, 1]
        ));
        append_attr(&mut nested, DATA_VERDICT, &[]).unwrap();
        let mut mixed = Vec::new();
        append_attr(&mut mixed, IMMEDIATE_DREG, &1u32.to_be_bytes()).unwrap();
        append_attr(&mut mixed, IMMEDIATE_DATA, &nested).unwrap();
        assert!(matches!(immediate_read(&mixed), Err(SystemError::EINVAL)));
    }

    #[test]
    fn byteorder_and_range_require_complete_unique_nested_attributes() {
        let mut byteorder = Vec::new();
        for (kind, value) in [
            (BYTEORDER_SREG, 8u32),
            (BYTEORDER_DREG, 8),
            (BYTEORDER_OP, 1),
            (BYTEORDER_LEN, 4),
            (BYTEORDER_SIZE, 4),
        ] {
            append_attr(&mut byteorder, kind, &value.to_be_bytes()).unwrap();
        }
        assert!(matches!(
            byteorder_read(&byteorder),
            Ok(NftExpressionInput::Byteorder {
                sreg: 8,
                dreg: 8,
                op: 1,
                len: 4,
                size: 4
            })
        ));
        append_attr(&mut byteorder, BYTEORDER_OP, &0u32.to_be_bytes()).unwrap();
        assert!(matches!(
            byteorder_read(&byteorder),
            Err(SystemError::EINVAL)
        ));

        let mut range = Vec::new();
        append_attr(&mut range, RANGE_SREG, &8u32.to_be_bytes()).unwrap();
        append_attr(&mut range, RANGE_OP, &1u32.to_be_bytes()).unwrap();
        let mut value = Vec::new();
        append_attr(&mut value, DATA_VALUE, &[127, 0, 0, 2]).unwrap();
        append_attr(&mut range, RANGE_FROM_DATA, &value).unwrap();
        assert!(matches!(range_read(&range), Err(SystemError::EINVAL)));
        value.clear();
        append_attr(&mut value, DATA_VALUE, &[127, 0, 0, 3]).unwrap();
        append_attr(&mut range, RANGE_TO_DATA, &value).unwrap();
        assert!(matches!(
            range_read(&range),
            Ok(NftExpressionInput::Range { sreg: 8, op: 1, from, to })
                if from == [127, 0, 0, 2] && to == [127, 0, 0, 3]
        ));
    }

    #[test]
    fn meta_read_and_write_registers_are_mutually_exclusive() {
        let mut read = Vec::new();
        append_attr(&mut read, META_KEY, &3u32.to_be_bytes()).unwrap();
        append_attr(&mut read, META_DREG, &8u32.to_be_bytes()).unwrap();
        assert!(matches!(
            meta_read(&read),
            Ok(NftExpressionInput::Meta { key: 3, dreg: 8 })
        ));
        let mut write = Vec::new();
        append_attr(&mut write, META_KEY, &3u32.to_be_bytes()).unwrap();
        append_attr(&mut write, META_SREG, &8u32.to_be_bytes()).unwrap();
        assert!(matches!(
            meta_read(&write),
            Ok(NftExpressionInput::MetaSet { key: 3, sreg: 8 })
        ));
        append_attr(&mut write, META_DREG, &8u32.to_be_bytes()).unwrap();
        assert!(matches!(meta_read(&write), Err(SystemError::EINVAL)));
    }

    #[test]
    fn redirect_attrs_keep_port_flags_and_reject_unknown_bits() {
        let mut data = Vec::new();
        append_attr(&mut data, REDIR_REG_PROTO_MIN, &8u32.to_be_bytes()).unwrap();
        append_attr(&mut data, REDIR_FLAGS, &2u32.to_be_bytes()).unwrap();
        assert!(matches!(
            redirect_read(&data),
            Ok(NftExpressionInput::Redirect {
                flags: Some(2),
                proto_min_reg: Some(8),
                proto_max_reg: None,
            })
        ));
        append_attr(&mut data, REDIR_FLAGS, &4u32.to_be_bytes()).unwrap();
        assert!(matches!(redirect_read(&data), Err(SystemError::EINVAL)));
        let mut unsupported = Vec::new();
        append_attr(&mut unsupported, REDIR_FLAGS, &0x80u32.to_be_bytes()).unwrap();
        assert!(matches!(
            redirect_read(&unsupported),
            Err(SystemError::EINVAL)
        ));
    }

    #[test]
    fn ct_state_requires_read_register_and_preserves_network_order() {
        let mut data = Vec::new();
        append_attr(&mut data, CT_KEY, &0u32.to_be_bytes()).unwrap();
        append_attr(&mut data, CT_DREG, &8u32.to_be_bytes()).unwrap();
        assert!(matches!(
            ct_read(&data),
            Ok(NftExpressionInput::Ct {
                key: 0,
                dreg: 8,
                direction: None,
            })
        ));
        append_attr(&mut data, CT_SREG, &9u32.to_be_bytes()).unwrap();
        assert!(matches!(
            ct_read(&data),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
    }

    #[test]
    fn xt_conntrack_state_match_keeps_revision_and_rejects_unknown_revision() {
        let mut data = Vec::new();
        append_attr(&mut data, MATCH_NAME, b"conntrack\0").unwrap();
        append_attr(&mut data, MATCH_REV, &2u32.to_be_bytes()).unwrap();
        let mut info = [0u8; 160];
        info[146..148].copy_from_slice(&1u16.to_ne_bytes());
        info[150..152].copy_from_slice(&8u16.to_ne_bytes());
        append_attr(&mut data, MATCH_INFO, &info).unwrap();
        assert!(matches!(
            xt_match(&data),
            Ok(NftExpressionInput::XtConntrack { revision: 2, info: payload })
                if payload == info
        ));
        let mut unsupported = Vec::new();
        append_attr(&mut unsupported, MATCH_NAME, b"conntrack\0").unwrap();
        append_attr(&mut unsupported, MATCH_REV, &4u32.to_be_bytes()).unwrap();
        append_attr(&mut unsupported, MATCH_INFO, &info).unwrap();
        assert!(matches!(xt_match(&unsupported), Err(SystemError::ENOENT)));
    }

    #[test]
    fn compat_query_reports_supported_conntrack_revisions() {
        for family in [2u8, 10] {
            let mut bytes = alloc::vec![0u8; HEADER_LEN + NFGEN_LEN];
            bytes[HEADER_LEN] = family;
            append_attr(&mut bytes, COMPAT_NAME, b"conntrack\0").unwrap();
            append_attr(&mut bytes, COMPAT_REV, &2u32.to_be_bytes()).unwrap();
            append_attr(&mut bytes, COMPAT_TYPE, &0u32.to_be_bytes()).unwrap();
            let len = bytes.len() as u32;
            bytes[..4].copy_from_slice(&len.to_ne_bytes());
            bytes[4..6].copy_from_slice(&(11u16 << 8).to_ne_bytes());
            let request = Request::parse(&bytes).unwrap();
            assert!(compat_match_reply(&request, 1).is_ok());
        }
    }

    #[test]
    fn compat_query_reports_ipv6_addrtype_revision_one_only() {
        for (revision, supported) in [(0, false), (1, true), (2, false)] {
            let mut bytes = alloc::vec![0u8; HEADER_LEN + NFGEN_LEN];
            bytes[HEADER_LEN] = 10;
            append_attr(&mut bytes, COMPAT_NAME, b"addrtype\0").unwrap();
            append_attr(&mut bytes, COMPAT_REV, &revision.to_be_bytes()).unwrap();
            append_attr(&mut bytes, COMPAT_TYPE, &0u32.to_be_bytes()).unwrap();
            let len = bytes.len() as u32;
            bytes[..4].copy_from_slice(&len.to_ne_bytes());
            bytes[4..6].copy_from_slice(&(11u16 << 8).to_ne_bytes());
            let request = Request::parse(&bytes).unwrap();
            assert_eq!(compat_match_reply(&request, 1).is_ok(), supported);
        }
    }

    #[test]
    fn nat_accepts_port_only_and_ignores_orphan_range_end() {
        let mut data = Vec::new();
        append_attr(&mut data, NAT_TYPE, &1u32.to_be_bytes()).unwrap();
        append_attr(&mut data, NAT_FAMILY, &2u32.to_be_bytes()).unwrap();
        append_attr(&mut data, NAT_REG_PROTO_MIN, &8u32.to_be_bytes()).unwrap();
        assert!(matches!(
            nat_read(&data),
            Ok(NftExpressionInput::Nat {
                nat_type: 1,
                family: 2,
                addr_min_reg: None,
                proto_min_reg: Some(8),
                ..
            })
        ));

        let mut orphan = Vec::new();
        append_attr(&mut orphan, NAT_TYPE, &1u32.to_be_bytes()).unwrap();
        append_attr(&mut orphan, NAT_FAMILY, &2u32.to_be_bytes()).unwrap();
        append_attr(&mut orphan, NAT_REG_ADDR_MAX, &8u32.to_be_bytes()).unwrap();
        assert!(matches!(nat_read(&orphan), Err(SystemError::EINVAL)));
        append_attr(&mut orphan, NAT_REG_PROTO_MIN, &9u32.to_be_bytes()).unwrap();
        assert!(matches!(
            nat_read(&orphan),
            Ok(NftExpressionInput::Nat {
                addr_min_reg: None,
                addr_max_reg: None,
                proto_min_reg: Some(9),
                ..
            })
        ));
    }

    #[test]
    fn masquerade_ignores_orphan_port_end() {
        let mut data = Vec::new();
        append_attr(&mut data, MASQ_REG_PROTO_MAX, &8u32.to_be_bytes()).unwrap();
        assert!(matches!(
            masq_read(&data),
            Ok(NftExpressionInput::Masq {
                proto_min_reg: None,
                proto_max_reg: None,
                ..
            })
        ));
    }

    #[test]
    fn target_keeps_validated_compat_info() {
        let mut data = Vec::new();
        append_attr(&mut data, MATCH_NAME, b"MASQUERADE\0").unwrap();
        append_attr(&mut data, MATCH_REV, &0u32.to_be_bytes()).unwrap();
        append_attr(&mut data, MATCH_INFO, &[1, 2, 3, 4]).unwrap();
        match xt_target(&data) {
            Ok(NftExpressionInput::XtTarget {
                name,
                revision,
                info,
            }) => {
                assert_eq!(name, b"MASQUERADE");
                assert_eq!(revision, 0);
                assert_eq!(info, [1, 2, 3, 4]);
            }
            _ => panic!("valid target expression was not decoded"),
        }
    }
}

pub(super) fn rule_attrs<'a>(request: &'a Request<'_>) -> Result<RuleAttrs<'a>, SystemError> {
    let mut attrs = RuleAttrs::default();
    for nla in NlaIter::new(&request.bytes[HEADER_LEN + NFGEN_LEN..]) {
        let nla = nla?;
        match nla.kind {
            RULE_TABLE if attrs.table.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RULE_TABLE => {}
            RULE_CHAIN if attrs.chain.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RULE_CHAIN => {}
            RULE_HANDLE if attrs.handle.replace(be_u64(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RULE_HANDLE => {}
            RULE_POSITION if attrs.position.replace(be_u64(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            RULE_POSITION => {}
            RULE_EXPRESSIONS => {
                if attrs.expressions.is_some() {
                    return Err(SystemError::EINVAL);
                }
                let mut compiled = Vec::new();
                for expression in NlaIter::new(nla.value) {
                    let expression = expression?;
                    if expression.kind != LIST_ELEM || compiled.len() >= 64 {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    compiled.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
                    compiled.push(rule_expression(expression.value)?);
                }
                attrs.expressions = Some(compiled);
            }
            RULE_COMPAT => {
                // Linux validates only NLA_NESTED's outer size here. Its
                // contents are consulted when an nft_compat expression is
                // present; native expressions ignore the opaque metadata.
                if !nla.value.is_empty() && nla.value.len() < 4 {
                    return Err(SystemError::EINVAL);
                }
                if attrs.compat.replace(nla.value).is_some() {
                    return Err(SystemError::EINVAL);
                }
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(attrs)
}

pub(super) fn set_attrs<'a>(request: &'a Request<'_>) -> Result<SetAttrs<'a>, SystemError> {
    let mut attrs = SetAttrs::default();
    for nla in NlaIter::new(&request.bytes[HEADER_LEN + NFGEN_LEN..]) {
        let nla = nla?;
        match nla.kind {
            SET_TABLE if attrs.table.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_TABLE => {}
            SET_NAME if attrs.name.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_NAME => {}
            SET_FLAGS => attrs.flags = be_u32(nla.value)?,
            SET_KEY_TYPE if attrs.key_type.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_KEY_TYPE => {}
            SET_KEY_LEN if attrs.key_len.replace(be_u32(nla.value)? as usize).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_KEY_LEN => {}
            SET_DATA_TYPE if attrs.data_type.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_DATA_TYPE => {}
            SET_DATA_LEN
                if attrs
                    .data_len
                    .replace(be_u32(nla.value)? as usize)
                    .is_some() =>
            {
                return Err(SystemError::EINVAL)
            }
            SET_DATA_LEN => {}
            SET_ID if attrs.id.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_ID => {}
            SET_HANDLE if attrs.handle.replace(be_u64(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_HANDLE => {}
            SET_USERDATA if nla.value.len() > 256 => return Err(SystemError::ERANGE),
            SET_USERDATA if attrs.userdata.replace(nla.value).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_USERDATA => {}
            SET_POLICY if be_u32(nla.value)? == 0 => {}
            SET_DESC if attrs.size.is_some() => return Err(SystemError::EINVAL),
            SET_DESC => {
                for field in NlaIter::new(nla.value) {
                    let field = field?;
                    if field.kind != SET_DESC_SIZE
                        || attrs.size.replace(be_u32(field.value)? as usize).is_some()
                    {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                }
            }
            SET_POLICY => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(attrs)
}

pub(super) fn set_elem_attrs<'a>(
    request: &'a Request<'_>,
) -> Result<SetElemAttrs<'a>, SystemError> {
    let mut attrs = SetElemAttrs::default();
    for nla in NlaIter::new(&request.bytes[HEADER_LEN + NFGEN_LEN..]) {
        let nla = nla?;
        match nla.kind {
            SET_ELEM_TABLE if attrs.table.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_ELEM_TABLE => {}
            SET_ELEM_SET if attrs.set.replace(nla_name(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_ELEM_SET => {}
            SET_ELEM_SET_ID if attrs.set_id.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL)
            }
            SET_ELEM_SET_ID => {}
            SET_ELEM_ELEMENTS => {
                if !attrs.elements.is_empty() {
                    return Err(SystemError::EINVAL);
                }
                for item in NlaIter::new(nla.value) {
                    let item = item?;
                    // libnftnl numbers elements 1, 2, ... within one
                    // NEWSETELEM request (unlike expression lists, which
                    // repeat NFTA_LIST_ELEM). Each item has the same nested
                    // element schema regardless of its list index.
                    if item.kind == 0 {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let (mut key, mut value, mut verdict, mut flags, mut key_end) =
                        (None, None, None, None, None);
                    for field in NlaIter::new(item.value) {
                        let field = field?;
                        match field.kind {
                            SET_ELEMENT_FLAGS => {
                                if flags.replace(be_u32(field.value)?).is_some() {
                                    return Err(SystemError::EINVAL);
                                }
                            }
                            SET_ELEMENT_KEY | SET_ELEMENT_DATA | SET_ELEMENT_KEY_END => {
                                let mut seen = false;
                                for data in NlaIter::new(field.value) {
                                    let data = data?;
                                    if seen {
                                        return Err(SystemError::EINVAL);
                                    }
                                    seen = true;
                                    match (field.kind, data.kind) {
                                        (SET_ELEMENT_KEY, DATA_VALUE)
                                            if key.replace(data.value).is_none() => {}
                                        (SET_ELEMENT_KEY_END, DATA_VALUE)
                                            if key_end.replace(data.value).is_none() => {}
                                        (SET_ELEMENT_DATA, DATA_VALUE)
                                            if value.replace(data.value).is_none() => {}
                                        (SET_ELEMENT_DATA, DATA_VERDICT)
                                            if verdict
                                                .replace(parse_verdict(data.value)?)
                                                .is_none() => {}
                                        _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                                    }
                                }
                            }
                            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                        }
                    }
                    attrs
                        .elements
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    attrs.elements.push(NftSetElementInput {
                        key: key.ok_or(SystemError::EINVAL)?,
                        value,
                        verdict,
                        flags: flags.unwrap_or(0),
                        key_end,
                    });
                }
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(attrs)
}

fn own_optional_name(name: Option<&[u8]>) -> Result<Option<Vec<u8>>, SystemError> {
    name.map(|name| {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| SystemError::ENOMEM)?;
        owned.extend_from_slice(name);
        Ok(owned)
    })
    .transpose()
}

fn nla_name(value: &[u8]) -> Result<&[u8], SystemError> {
    if value.is_empty() || value.len() > 256 {
        return Err(SystemError::ERANGE);
    }
    let end = value
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    if end == 0 || end >= 256 {
        return Err(SystemError::EINVAL);
    }
    Ok(&value[..end])
}

fn compat_name(value: &[u8], max_len: usize) -> Result<&[u8], SystemError> {
    let ends_with_nul = value.last() == Some(&0);
    if value.is_empty() || value.len() > max_len + usize::from(ends_with_nul) {
        return Err(SystemError::EINVAL);
    }
    // NLA_NUL_STRING requires a terminator within the policy's first
    // max_len + 1 bytes; bytes after that first NUL are ignored by xt.
    let end = value
        .iter()
        .take(max_len + 1)
        .position(|byte| *byte == 0)
        .ok_or(SystemError::EINVAL)?;
    Ok(&value[..end])
}

pub(super) fn compat_match_reply(
    request: &Request<'_>,
    port: u32,
) -> Result<NetfilterMessage, SystemError> {
    let family = request.bytes[HEADER_LEN];
    if !matches!(family, 2 | 10) {
        return Err(SystemError::EINVAL);
    }
    let mut name = None;
    let mut revision = None;
    let mut kind = None;
    for nla in NlaIter::new(&request.bytes[HEADER_LEN + NFGEN_LEN..]) {
        let nla = nla?;
        match nla.kind {
            COMPAT_NAME if name.replace(nla.value).is_some() => return Err(SystemError::EINVAL),
            COMPAT_NAME => {}
            COMPAT_REV if revision.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            COMPAT_REV => {}
            COMPAT_TYPE if kind.replace(be_u32(nla.value)?).is_some() => {
                return Err(SystemError::EINVAL);
            }
            COMPAT_TYPE => {}
            _ => {}
        }
    }
    let name = name.ok_or(SystemError::EINVAL)?;
    let revision = revision.ok_or(SystemError::EINVAL)?;
    let kind = kind.ok_or(SystemError::EINVAL)?;
    let name = compat_name(name, 31)?;
    if revision > u8::MAX as u32 {
        return Err(SystemError::ERANGE);
    }
    if kind > 1 {
        return Err(SystemError::EINVAL);
    }
    let wire_name: &[u8] = match name {
        b"MASQUERADE" if kind == 1 && revision == 0 && family == 2 => b"MASQUERADE\0",
        b"DNAT" if kind == 1 && revision == 0 && family == 2 => b"DNAT\0",
        b"SNAT" if kind == 1 && revision == 0 && family == 2 => b"SNAT\0",
        b"DNAT" if kind == 1 && revision == 2 && family == 10 => b"DNAT\0",
        b"MASQUERADE" | b"DNAT" | b"SNAT" if kind == 1 => return Err(SystemError::EPROTONOSUPPORT),
        _ if kind == 1 => return Err(SystemError::ENOENT),
        b"tcp" if revision == 0 && family == 2 => b"tcp\0",
        b"addrtype" if revision == 1 => b"addrtype\0",
        b"conntrack" if (1..=3).contains(&revision) => b"conntrack\0",
        b"tcp" if family == 10 => return Err(SystemError::ENOENT),
        b"tcp" | b"addrtype" | b"conntrack" => {
            return Err(SystemError::EPROTONOSUPPORT);
        }
        _ => return Err(SystemError::ENOENT),
    };

    let mut bytes = Vec::new();
    bytes
        .try_reserve(HEADER_LEN + NFGEN_LEN + 24)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.resize(HEADER_LEN, 0);
    bytes.push(family);
    bytes.push(0);
    bytes.extend_from_slice(&0u16.to_be_bytes());
    append_attr(&mut bytes, COMPAT_NAME, wire_name)?;
    append_attr(&mut bytes, COMPAT_REV, &revision.to_be_bytes())?;
    append_attr(&mut bytes, COMPAT_TYPE, &0u32.to_be_bytes())?;
    let message_len = bytes.len() as u32;
    bytes[0..4].copy_from_slice(&message_len.to_ne_bytes());
    bytes[4..6].copy_from_slice(&request.kind().to_ne_bytes());
    bytes[6..8].copy_from_slice(&2u16.to_ne_bytes()); // NLM_F_MULTI for unicast reply
    bytes[8..12].copy_from_slice(&request.bytes[8..12]);
    bytes[12..16].copy_from_slice(&port.to_ne_bytes());
    NetfilterMessage::new(bytes)
}

pub(super) fn chain_attrs<'a>(request: &'a Request<'_>) -> Result<ChainAttrs<'a>, SystemError> {
    let mut attrs = ChainAttrs::default();
    for nla in NlaIter::new(&request.bytes[HEADER_LEN + NFGEN_LEN..]) {
        let Nla { kind, value } = nla?;
        match kind {
            CHAIN_TABLE => attrs.table = Some(nla_name(value)?),
            CHAIN_NAME => attrs.name = Some(nla_name(value)?),
            CHAIN_TYPE => attrs.chain_type = Some(nla_name(value)?),
            CHAIN_HANDLE => {
                if value.len() < 8 {
                    return Err(SystemError::ERANGE);
                }
                attrs.handle = Some(u64::from_be_bytes(value[..8].try_into().unwrap()));
            }
            CHAIN_POLICY => {
                if value.len() < 4 {
                    return Err(SystemError::ERANGE);
                }
                attrs.policy = Some(match u32::from_be_bytes(value[..4].try_into().unwrap()) {
                    0 => NftVerdict::Drop,
                    1 => NftVerdict::Accept,
                    _ => return Err(SystemError::EINVAL),
                });
            }
            CHAIN_HOOK => {
                let mut hooknum = None;
                let mut priority = None;
                for nested in NlaIter::new(value) {
                    let Nla { kind, value } = nested?;
                    match kind {
                        HOOK_NUM | HOOK_PRIORITY if value.len() < 4 => {
                            return Err(SystemError::ERANGE)
                        }
                        HOOK_NUM => {
                            hooknum = Some(u32::from_be_bytes(value[..4].try_into().unwrap()))
                        }
                        HOOK_PRIORITY => {
                            priority = Some(i32::from_be_bytes(value[..4].try_into().unwrap()))
                        }
                        // Device-bound hooks are not executable in this path.
                        3 | 4 => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                        _ => {}
                    }
                }
                attrs.hook = Some((
                    hooknum.ok_or(SystemError::ENOENT)?,
                    priority.ok_or(SystemError::ENOENT)?,
                ));
            }
            CHAIN_FLAGS => {
                if value.len() < 4 {
                    return Err(SystemError::ERANGE);
                }
                attrs.flags = u32::from_be_bytes(value[..4].try_into().unwrap());
            }
            CHAIN_USE if value.len() < 4 => return Err(SystemError::ERANGE),
            _ => {}
        }
    }
    Ok(attrs)
}

pub(super) fn table_attrs<'a>(request: &'a Request<'_>) -> Result<TableAttrs<'a>, SystemError> {
    let mut attrs = TableAttrs::default();
    for nla in NlaIter::new(&request.bytes[HEADER_LEN + NFGEN_LEN..]) {
        let Nla { kind, value } = nla?;
        match kind {
            ATTR_NAME => {
                attrs.name = Some(nla_name(value)?);
            }
            ATTR_FLAGS if value.len() < 4 => return Err(SystemError::ERANGE),
            ATTR_FLAGS => {
                attrs.flags = u32::from_be_bytes(value[..4].try_into().unwrap());
            }
            ATTR_HANDLE if value.len() < 8 => return Err(SystemError::ERANGE),
            ATTR_HANDLE => {
                attrs.handle = Some(u64::from_be_bytes(value[..8].try_into().unwrap()));
            }
            ATTR_USERDATA if value.len() > 256 => return Err(SystemError::ERANGE),
            ATTR_USERDATA => attrs.userdata = value,
            _ => {}
        }
    }
    Ok(attrs)
}

fn append_attr(bytes: &mut Vec<u8>, kind: u16, value: &[u8]) -> Result<(), SystemError> {
    let size = 4usize
        .checked_add(value.len())
        .ok_or(SystemError::EMSGSIZE)?;
    let aligned = size.checked_add(3).ok_or(SystemError::EMSGSIZE)? & !3;
    let wire_len = u16::try_from(size).map_err(|_| SystemError::EMSGSIZE)?;
    bytes
        .try_reserve(aligned)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.extend_from_slice(&wire_len.to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(value);
    bytes.resize(bytes.len() + aligned - size, 0);
    Ok(())
}

pub(super) fn table_message(
    table: &NftTable,
    generation: u32,
    sequence: u32,
    port: u32,
    kind: u16,
    flags: u16,
    use_count_override: Option<u32>,
) -> Result<NetfilterMessage, SystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve(HEADER_LEN + NFGEN_LEN)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.resize(HEADER_LEN, 0);
    bytes.push(table.family);
    bytes.push(0);
    bytes.extend_from_slice(&(generation as u16).to_be_bytes());

    // The name on the wire is a NUL-terminated NLA_STRING.
    let mut name = Vec::new();
    name.try_reserve_exact(table.name.len() + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    name.extend_from_slice(&table.name);
    name.push(0);
    append_attr(&mut bytes, ATTR_NAME, &name)?;
    append_attr(
        &mut bytes,
        ATTR_USE,
        &use_count_override.unwrap_or(table.use_count).to_be_bytes(),
    )?;
    if bytes.len() & 7 != 0 {
        append_attr(&mut bytes, ATTR_PAD, &[])?;
    }
    append_attr(&mut bytes, ATTR_HANDLE, &table.handle.to_be_bytes())?;
    append_attr(&mut bytes, ATTR_FLAGS, &table.flags.to_be_bytes())?;
    if !table.userdata.is_empty() {
        append_attr(&mut bytes, ATTR_USERDATA, &table.userdata)?;
    }
    let message_len = bytes.len() as u32;
    bytes[..4].copy_from_slice(&message_len.to_ne_bytes());
    bytes[4..6].copy_from_slice(&kind.to_ne_bytes());
    bytes[6..8].copy_from_slice(&flags.to_ne_bytes());
    bytes[8..12].copy_from_slice(&sequence.to_ne_bytes());
    bytes[12..16].copy_from_slice(&port.to_ne_bytes());
    NetfilterMessage::new(bytes)
}

pub(super) fn set_message(
    table: &NftTable,
    set: &NftSet,
    generation: u32,
    sequence: u32,
    port: u32,
    kind: u16,
    flags: u16,
) -> Result<NetfilterMessage, SystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve(HEADER_LEN + NFGEN_LEN)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.resize(HEADER_LEN, 0);
    bytes.push(table.family);
    bytes.push(0);
    bytes.extend_from_slice(&(generation as u16).to_be_bytes());
    let mut name = Vec::new();
    name.try_reserve_exact(table.name.len().max(set.name.len()) + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    name.extend_from_slice(&table.name);
    name.push(0);
    append_attr(&mut bytes, SET_TABLE, &name)?;
    name.clear();
    name.extend_from_slice(&set.name);
    name.push(0);
    append_attr(&mut bytes, SET_NAME, &name)?;
    append_attr(&mut bytes, SET_FLAGS, &set.flags.to_be_bytes())?;
    append_attr(&mut bytes, SET_KEY_TYPE, &set.key_type.to_be_bytes())?;
    append_attr(&mut bytes, SET_KEY_LEN, &(set.key_len as u32).to_be_bytes())?;
    if let (Some(data_type), Some(data_len)) = (set.data_type, set.data_len) {
        append_attr(&mut bytes, SET_DATA_TYPE, &data_type.to_be_bytes())?;
        append_attr(&mut bytes, SET_DATA_LEN, &(data_len as u32).to_be_bytes())?;
    }
    if let Some(size) = set.size {
        let mut descriptor = Vec::new();
        append_attr(&mut descriptor, SET_DESC_SIZE, &(size as u32).to_be_bytes())?;
        append_attr(&mut bytes, SET_DESC, &descriptor)?;
    }
    if !set.userdata.is_empty() {
        append_attr(&mut bytes, SET_USERDATA, &set.userdata)?;
    }
    if bytes.len() & 7 != 0 {
        append_attr(&mut bytes, 14, &[])?;
    }
    append_attr(&mut bytes, SET_HANDLE, &set.handle.to_be_bytes())?;
    finish_nft_message(bytes, sequence, port, kind, flags)
}

pub(super) struct SetElementMessageMeta {
    pub(super) generation: u32,
    pub(super) sequence: u32,
    pub(super) port: u32,
    pub(super) kind: u16,
    pub(super) flags: u16,
}

pub(super) fn set_elem_message(
    table: &NftTable,
    set: &NftSet,
    key: &[u8],
    value: Option<&[u8]>,
    verdict: Option<NftRuleVerdict>,
    element_flags: u32,
    meta: SetElementMessageMeta,
) -> Result<NetfilterMessage, SystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve(HEADER_LEN + NFGEN_LEN)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.resize(HEADER_LEN, 0);
    bytes.push(table.family);
    bytes.push(0);
    bytes.extend_from_slice(&(meta.generation as u16).to_be_bytes());
    let mut name = Vec::new();
    name.try_reserve_exact(table.name.len().max(set.name.len()) + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    name.extend_from_slice(&table.name);
    name.push(0);
    append_attr(&mut bytes, SET_ELEM_TABLE, &name)?;
    name.clear();
    name.extend_from_slice(&set.name);
    name.push(0);
    append_attr(&mut bytes, SET_ELEM_SET, &name)?;
    let mut key_data = Vec::new();
    append_attr(&mut key_data, DATA_VALUE, key)?;
    let mut element = Vec::new();
    append_attr(&mut element, SET_ELEMENT_KEY, &key_data)?;
    if let Some(mapped) = value {
        let mut data = Vec::new();
        append_attr(&mut data, DATA_VALUE, mapped)?;
        append_attr(&mut element, SET_ELEMENT_DATA, &data)?;
    }
    if let Some(verdict) = verdict {
        let code = match verdict {
            NftRuleVerdict::Drop => 0,
            NftRuleVerdict::Accept => 1,
            NftRuleVerdict::Continue => (-1i32) as u32,
            NftRuleVerdict::Return => (-5i32) as u32,
            NftRuleVerdict::Jump(_) => (-3i32) as u32,
            NftRuleVerdict::Goto(_) => (-4i32) as u32,
        };
        let mut verdict_data = Vec::new();
        append_attr(&mut verdict_data, VERDICT_CODE, &code.to_be_bytes())?;
        if let NftRuleVerdict::Jump(handle) | NftRuleVerdict::Goto(handle) = verdict {
            let target = table
                .chains
                .iter()
                .find(|chain| chain.handle == handle)
                .ok_or(SystemError::ENOENT)?;
            let mut name = Vec::new();
            name.try_reserve_exact(target.name.len() + 1)
                .map_err(|_| SystemError::ENOMEM)?;
            name.extend_from_slice(&target.name);
            name.push(0);
            append_attr(&mut verdict_data, VERDICT_CHAIN, &name)?;
        }
        let mut data = Vec::new();
        append_attr(&mut data, DATA_VERDICT, &verdict_data)?;
        append_attr(&mut element, SET_ELEMENT_DATA, &data)?;
    }
    if element_flags != 0 {
        append_attr(
            &mut element,
            SET_ELEMENT_FLAGS,
            &element_flags.to_be_bytes(),
        )?;
    }
    let mut elements = Vec::new();
    append_attr(&mut elements, LIST_ELEM, &element)?;
    append_attr(&mut bytes, SET_ELEM_ELEMENTS, &elements)?;
    finish_nft_message(bytes, meta.sequence, meta.port, meta.kind, meta.flags)
}

fn finish_nft_message(
    mut bytes: Vec<u8>,
    sequence: u32,
    port: u32,
    kind: u16,
    flags: u16,
) -> Result<NetfilterMessage, SystemError> {
    let len = u32::try_from(bytes.len()).map_err(|_| SystemError::EMSGSIZE)?;
    bytes[..4].copy_from_slice(&len.to_ne_bytes());
    bytes[4..6].copy_from_slice(&kind.to_ne_bytes());
    bytes[6..8].copy_from_slice(&flags.to_ne_bytes());
    bytes[8..12].copy_from_slice(&sequence.to_ne_bytes());
    bytes[12..16].copy_from_slice(&port.to_ne_bytes());
    NetfilterMessage::new(bytes)
}

pub(super) fn chain_message(
    table: &NftTable,
    chain: &NftChain,
    generation: u32,
    sequence: u32,
    port: u32,
    kind: u16,
    flags: u16,
) -> Result<NetfilterMessage, SystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve(HEADER_LEN + NFGEN_LEN)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.resize(HEADER_LEN, 0);
    bytes.push(table.family);
    bytes.push(0);
    bytes.extend_from_slice(&(generation as u16).to_be_bytes());

    let mut name = Vec::new();
    name.try_reserve_exact(table.name.len().max(chain.name.len()) + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    name.extend_from_slice(&table.name);
    name.push(0);
    append_attr(&mut bytes, CHAIN_TABLE, &name)?;
    name.clear();
    name.extend_from_slice(&chain.name);
    name.push(0);
    append_attr(&mut bytes, CHAIN_NAME, &name)?;
    if bytes.len() & 7 != 0 {
        append_attr(&mut bytes, CHAIN_PAD, &[])?;
    }
    append_attr(&mut bytes, CHAIN_HANDLE, &chain.handle.to_be_bytes())?;
    if let Some(base) = chain.base {
        let mut hook = Vec::new();
        append_attr(&mut hook, HOOK_NUM, &(base.hook as u32).to_be_bytes())?;
        append_attr(&mut hook, HOOK_PRIORITY, &base.priority.to_be_bytes())?;
        append_attr(&mut bytes, CHAIN_HOOK, &hook)?;
        let policy: u32 = if base.policy == NftVerdict::Drop {
            0
        } else {
            1
        };
        append_attr(&mut bytes, CHAIN_POLICY, &policy.to_be_bytes())?;
        let chain_type = match base.chain_type {
            NftChainType::Filter => b"filter\0".as_slice(),
            NftChainType::Nat => b"nat\0".as_slice(),
        };
        append_attr(&mut bytes, CHAIN_TYPE, chain_type)?;
    }
    append_attr(
        &mut bytes,
        CHAIN_USE,
        &table.chain_use_count(chain).to_be_bytes(),
    )?;
    let message_len = u32::try_from(bytes.len()).map_err(|_| SystemError::EMSGSIZE)?;
    bytes[..4].copy_from_slice(&message_len.to_ne_bytes());
    bytes[4..6].copy_from_slice(&kind.to_ne_bytes());
    bytes[6..8].copy_from_slice(&flags.to_ne_bytes());
    bytes[8..12].copy_from_slice(&sequence.to_ne_bytes());
    bytes[12..16].copy_from_slice(&port.to_ne_bytes());
    NetfilterMessage::new(bytes)
}

pub(super) fn rule_message(
    scope: (&NftTable, &NftChain),
    rule_with_position: (&NftRule, Option<u64>),
    generation: u32,
    sequence: u32,
    port: u32,
    kind: u16,
    flags: u16,
) -> Result<NetfilterMessage, SystemError> {
    let (table, chain) = scope;
    let (rule, position) = rule_with_position;
    let mut bytes = Vec::new();
    bytes
        .try_reserve(HEADER_LEN + NFGEN_LEN)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.resize(HEADER_LEN, 0);
    bytes.push(table.family);
    bytes.push(0);
    bytes.extend_from_slice(&(generation as u16).to_be_bytes());

    let mut name = Vec::new();
    name.try_reserve_exact(table.name.len().max(chain.name.len()) + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    name.extend_from_slice(&table.name);
    name.push(0);
    append_attr(&mut bytes, RULE_TABLE, &name)?;
    name.clear();
    name.extend_from_slice(&chain.name);
    name.push(0);
    append_attr(&mut bytes, RULE_CHAIN, &name)?;
    if bytes.len() & 7 != 0 {
        append_attr(&mut bytes, RULE_PAD, &[])?;
    }
    append_attr(&mut bytes, RULE_HANDLE, &rule.handle.to_be_bytes())?;
    if kind != super::NFT_MSG_DELRULE {
        if let Some(position) = position {
            if bytes.len() & 7 != 0 {
                append_attr(&mut bytes, RULE_PAD, &[])?;
            }
            append_attr(&mut bytes, RULE_POSITION, &position.to_be_bytes())?;
        }
    }

    let mut expressions = Vec::new();
    for compiled in rule.expressions() {
        let mut data = Vec::new();
        let name: &[u8] = match compiled {
            NftExpression::ImmediateData { dreg, data: value } => {
                let mut nested = Vec::new();
                append_attr(&mut nested, DATA_VALUE, value)?;
                append_attr(
                    &mut data,
                    IMMEDIATE_DREG,
                    &uapi_register(*dreg).to_be_bytes(),
                )?;
                append_attr(&mut data, IMMEDIATE_DATA, &nested)?;
                b"immediate\0"
            }
            NftExpression::Lookup {
                set_handle,
                sreg,
                dreg,
                verdict_map,
                invert,
                ..
            } => {
                let set = table
                    .sets
                    .iter()
                    .find(|item| item.handle == *set_handle)
                    .ok_or(SystemError::ENOENT)?;
                let mut set_name = Vec::new();
                set_name
                    .try_reserve_exact(set.name.len() + 1)
                    .map_err(|_| SystemError::ENOMEM)?;
                set_name.extend_from_slice(&set.name);
                set_name.push(0);
                append_attr(&mut data, LOOKUP_SET, &set_name)?;
                append_attr(&mut data, LOOKUP_SREG, &uapi_register(*sreg).to_be_bytes())?;
                if let Some(dreg) = dreg {
                    let register = if *verdict_map {
                        0
                    } else {
                        uapi_register(*dreg)
                    };
                    append_attr(&mut data, LOOKUP_DREG, &register.to_be_bytes())?;
                }
                if *invert {
                    append_attr(&mut data, LOOKUP_FLAGS, &1u32.to_be_bytes())?;
                }
                b"lookup\0"
            }
            NftExpression::Immediate(verdict) => {
                let mut verdict_data = Vec::new();
                let code = match verdict {
                    NftRuleVerdict::Drop => 0,
                    NftRuleVerdict::Accept => 1,
                    NftRuleVerdict::Continue => (-1i32) as u32,
                    NftRuleVerdict::Jump(_) => (-3i32) as u32,
                    NftRuleVerdict::Goto(_) => (-4i32) as u32,
                    NftRuleVerdict::Return => (-5i32) as u32,
                };
                append_attr(&mut verdict_data, VERDICT_CODE, &code.to_be_bytes())?;
                if let NftRuleVerdict::Jump(target) | NftRuleVerdict::Goto(target) = verdict {
                    let target = table
                        .chains
                        .iter()
                        .find(|chain| chain.handle == *target)
                        .ok_or(SystemError::ENOENT)?;
                    let mut target_name = Vec::new();
                    target_name
                        .try_reserve_exact(target.name.len() + 1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    target_name.extend_from_slice(&target.name);
                    target_name.push(0);
                    append_attr(&mut verdict_data, VERDICT_CHAIN, &target_name)?;
                }
                let mut value = Vec::new();
                append_attr(&mut value, DATA_VERDICT, &verdict_data)?;
                append_attr(&mut data, IMMEDIATE_DREG, &0u32.to_be_bytes())?;
                append_attr(&mut data, IMMEDIATE_DATA, &value)?;
                b"immediate\0"
            }
            NftExpression::Payload {
                base,
                offset,
                len,
                dreg,
            } => {
                append_attr(&mut data, PAYLOAD_DREG, &uapi_register(*dreg).to_be_bytes())?;
                let base = match base {
                    NftPayloadBase::Network => 1u32,
                    NftPayloadBase::Transport => 2u32,
                };
                append_attr(&mut data, PAYLOAD_BASE, &base.to_be_bytes())?;
                append_attr(&mut data, PAYLOAD_OFFSET, &(*offset as u32).to_be_bytes())?;
                append_attr(&mut data, PAYLOAD_LEN, &(*len as u32).to_be_bytes())?;
                b"payload\0"
            }
            NftExpression::Meta { key, dreg } => {
                append_attr(&mut data, META_DREG, &uapi_register(*dreg).to_be_bytes())?;
                append_attr(&mut data, META_KEY, &key.uapi().to_be_bytes())?;
                b"meta\0"
            }
            NftExpression::MetaSetMark { sreg } => {
                append_attr(&mut data, META_SREG, &uapi_register(*sreg).to_be_bytes())?;
                append_attr(&mut data, META_KEY, &NftMetaKey::Mark.uapi().to_be_bytes())?;
                b"meta\0"
            }
            NftExpression::FibDaddrType { dreg } => {
                append_attr(&mut data, FIB_DREG, &uapi_register(*dreg).to_be_bytes())?;
                append_attr(&mut data, FIB_RESULT, &3u32.to_be_bytes())?;
                append_attr(&mut data, FIB_FLAGS, &2u32.to_be_bytes())?;
                b"fib\0"
            }
            NftExpression::CtState { dreg } => {
                append_attr(&mut data, CT_DREG, &uapi_register(*dreg).to_be_bytes())?;
                append_attr(&mut data, CT_KEY, &0u32.to_be_bytes())?;
                b"ct\0"
            }
            NftExpression::Nat {
                side,
                family,
                addr_min,
                addr_max,
                port_min,
                port_max,
            } => {
                let nat_type = if *side == crate::net::conntrack::NatManipSide::Source {
                    0u32
                } else {
                    1u32
                };
                append_attr(&mut data, NAT_TYPE, &nat_type.to_be_bytes())?;
                append_attr(&mut data, NAT_FAMILY, &u32::from(*family).to_be_bytes())?;
                for (kind, reg) in [
                    (NAT_REG_ADDR_MIN, *addr_min),
                    (NAT_REG_ADDR_MAX, (*addr_max).or(*addr_min)),
                    (NAT_REG_PROTO_MIN, *port_min),
                    (NAT_REG_PROTO_MAX, (*port_max).or(*port_min)),
                ] {
                    if let Some(reg) = reg {
                        append_attr(&mut data, kind, &uapi_register(reg).to_be_bytes())?;
                    }
                }
                let flags = u32::from(addr_min.is_some()) | (u32::from(port_min.is_some()) << 1);
                if flags != 0 {
                    append_attr(&mut data, NAT_FLAGS, &flags.to_be_bytes())?;
                }
                b"nat\0"
            }
            NftExpression::Masquerade { port_min, port_max } => {
                for (kind, reg) in [
                    (MASQ_REG_PROTO_MIN, *port_min),
                    (MASQ_REG_PROTO_MAX, (*port_max).or(*port_min)),
                ] {
                    if let Some(reg) = reg {
                        append_attr(&mut data, kind, &uapi_register(reg).to_be_bytes())?;
                    }
                }
                b"masq\0"
            }
            NftExpression::Redirect {
                flags,
                port_min,
                port_max,
            } => {
                for (kind, reg) in [
                    (REDIR_REG_PROTO_MIN, *port_min),
                    (REDIR_REG_PROTO_MAX, (*port_max).or(*port_min)),
                ] {
                    if let Some(reg) = reg {
                        append_attr(&mut data, kind, &uapi_register(reg).to_be_bytes())?;
                    }
                }
                if *flags != 0 {
                    append_attr(&mut data, REDIR_FLAGS, &flags.to_be_bytes())?;
                }
                b"redir\0"
            }
            NftExpression::XtNatTarget {
                name,
                revision,
                info,
                ..
            } => {
                let mut wire_name = Vec::new();
                wire_name
                    .try_reserve_exact(name.len() + 1)
                    .map_err(|_| SystemError::ENOMEM)?;
                wire_name.extend_from_slice(name);
                wire_name.push(0);
                append_attr(&mut data, MATCH_NAME, &wire_name)?;
                append_attr(&mut data, MATCH_REV, &revision.to_be_bytes())?;
                append_attr(&mut data, MATCH_INFO, info)?;
                b"target\0"
            }
            NftExpression::Cmp {
                sreg,
                op,
                data: value,
            } => {
                let code: u32 = match op {
                    NftCmpOp::Eq => 0,
                    NftCmpOp::Neq => 1,
                    NftCmpOp::Lt => 2,
                    NftCmpOp::Lte => 3,
                    NftCmpOp::Gt => 4,
                    NftCmpOp::Gte => 5,
                };
                append_attr(&mut data, CMP_SREG, &uapi_register(*sreg).to_be_bytes())?;
                append_attr(&mut data, CMP_OP, &code.to_be_bytes())?;
                let mut nested = Vec::new();
                append_attr(&mut nested, DATA_VALUE, value)?;
                append_attr(&mut data, CMP_DATA, &nested)?;
                b"cmp\0"
            }
            NftExpression::Byteorder {
                sreg,
                dreg,
                op,
                len,
                size,
            } => {
                append_attr(
                    &mut data,
                    BYTEORDER_SREG,
                    &uapi_register(*sreg).to_be_bytes(),
                )?;
                append_attr(
                    &mut data,
                    BYTEORDER_DREG,
                    &uapi_register(*dreg).to_be_bytes(),
                )?;
                let op: u32 = match op {
                    NftByteorderOp::NetworkToHost => 0,
                    NftByteorderOp::HostToNetwork => 1,
                };
                append_attr(&mut data, BYTEORDER_OP, &op.to_be_bytes())?;
                append_attr(&mut data, BYTEORDER_LEN, &(*len as u32).to_be_bytes())?;
                append_attr(&mut data, BYTEORDER_SIZE, &(*size as u32).to_be_bytes())?;
                b"byteorder\0"
            }
            NftExpression::Range { sreg, op, from, to } => {
                append_attr(&mut data, RANGE_SREG, &uapi_register(*sreg).to_be_bytes())?;
                let op: u32 = match op {
                    NftRangeOp::Eq => 0,
                    NftRangeOp::Neq => 1,
                };
                append_attr(&mut data, RANGE_OP, &op.to_be_bytes())?;
                let mut nested = Vec::new();
                append_attr(&mut nested, DATA_VALUE, from)?;
                append_attr(&mut data, RANGE_FROM_DATA, &nested)?;
                nested.clear();
                append_attr(&mut nested, DATA_VALUE, to)?;
                append_attr(&mut data, RANGE_TO_DATA, &nested)?;
                b"range\0"
            }
            NftExpression::Counter(counter) => {
                let (bytes, packets) = counter.snapshot();
                append_attr(&mut data, COUNTER_BYTES, &bytes.to_be_bytes())?;
                append_attr(&mut data, COUNTER_PACKETS, &packets.to_be_bytes())?;
                b"counter\0"
            }
            NftExpression::XtTcp(tcp) => {
                append_attr(&mut data, MATCH_NAME, b"tcp\0")?;
                append_attr(&mut data, MATCH_REV, &0u32.to_be_bytes())?;
                append_attr(&mut data, MATCH_INFO, &tcp.info())?;
                b"match\0"
            }
            NftExpression::XtAddrtype(matcher) => {
                append_attr(&mut data, MATCH_NAME, b"addrtype\0")?;
                append_attr(&mut data, MATCH_REV, &1u32.to_be_bytes())?;
                append_attr(&mut data, MATCH_INFO, &matcher.info())?;
                b"match\0"
            }
            NftExpression::XtConntrack(matcher) => {
                append_attr(&mut data, MATCH_NAME, b"conntrack\0")?;
                append_attr(&mut data, MATCH_REV, &matcher.revision().to_be_bytes())?;
                append_attr(&mut data, MATCH_INFO, matcher.info())?;
                b"match\0"
            }
            NftExpression::Bitwise {
                sreg,
                dreg,
                len,
                operation,
            } => {
                append_attr(&mut data, BITWISE_SREG, &uapi_register(*sreg).to_be_bytes())?;
                append_attr(&mut data, BITWISE_DREG, &uapi_register(*dreg).to_be_bytes())?;
                append_attr(&mut data, BITWISE_LEN, &(*len as u32).to_be_bytes())?;
                let op = match operation {
                    NftBitwiseOperation::Bool { mask, xor } => {
                        let mut nested = Vec::new();
                        append_attr(&mut nested, DATA_VALUE, mask)?;
                        append_attr(&mut data, BITWISE_MASK, &nested)?;
                        nested.clear();
                        append_attr(&mut nested, DATA_VALUE, xor)?;
                        append_attr(&mut data, BITWISE_XOR, &nested)?;
                        0u32
                    }
                    NftBitwiseOperation::Lshift(shift) | NftBitwiseOperation::Rshift(shift) => {
                        let mut nested = Vec::new();
                        append_attr(&mut nested, DATA_VALUE, &shift.to_ne_bytes())?;
                        append_attr(&mut data, BITWISE_DATA, &nested)?;
                        if matches!(operation, NftBitwiseOperation::Lshift(_)) {
                            1
                        } else {
                            2
                        }
                    }
                };
                append_attr(&mut data, BITWISE_OP, &op.to_be_bytes())?;
                b"bitwise\0"
            }
        };
        let mut expression = Vec::new();
        append_attr(&mut expression, EXPR_NAME, name)?;
        append_attr(&mut expression, EXPR_DATA, &data)?;
        append_attr(&mut expressions, LIST_ELEM, &expression)?;
    }
    append_attr(&mut bytes, RULE_EXPRESSIONS, &expressions)?;

    let message_len = u32::try_from(bytes.len()).map_err(|_| SystemError::EMSGSIZE)?;
    bytes[..4].copy_from_slice(&message_len.to_ne_bytes());
    bytes[4..6].copy_from_slice(&kind.to_ne_bytes());
    bytes[6..8].copy_from_slice(&flags.to_ne_bytes());
    bytes[8..12].copy_from_slice(&sequence.to_ne_bytes());
    bytes[12..16].copy_from_slice(&port.to_ne_bytes());
    NetfilterMessage::new(bytes)
}

fn uapi_register(offset: usize) -> u32 {
    if offset.is_multiple_of(16) {
        (offset / 16 + 1) as u32
    } else {
        (offset / 4 + 8) as u32
    }
}

fn done_message(
    sequence: u32,
    port: u32,
    interrupted: bool,
) -> Result<NetfilterMessage, SystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(HEADER_LEN + 4)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.extend_from_slice(&((HEADER_LEN + 4) as u32).to_ne_bytes());
    bytes.extend_from_slice(&3u16.to_ne_bytes()); // NLMSG_DONE
    let flags = NLM_F_MULTI | (if interrupted { NLM_F_DUMP_INTR } else { 0 });
    bytes.extend_from_slice(&flags.to_ne_bytes());
    bytes.extend_from_slice(&sequence.to_ne_bytes());
    bytes.extend_from_slice(&port.to_ne_bytes());
    bytes.extend_from_slice(&0i32.to_ne_bytes());
    NetfilterMessage::new(bytes)
}

impl TableDumpSession {
    fn next(
        &self,
        snapshot: &RulesetSnapshot,
        port: u32,
    ) -> Result<(NetfilterMessage, usize, bool), SystemError> {
        let interrupted = snapshot.generation != self.generation;
        let mut cursor = self.cursor;
        while let Some(table) = snapshot.tables.get(cursor) {
            cursor += 1;
            if self.family == 0 || self.family == table.family {
                return Ok((
                    table_message(
                        table,
                        snapshot.generation,
                        self.sequence,
                        port,
                        NFT_MSG_NEWTABLE,
                        NLM_F_MULTI | if interrupted { NLM_F_DUMP_INTR } else { 0 },
                        None,
                    )?,
                    cursor,
                    false,
                ));
            }
        }
        Ok((
            done_message(self.sequence, port, interrupted)?,
            cursor,
            true,
        ))
    }
}

impl ChainDumpSession {
    fn next(
        &self,
        snapshot: &RulesetSnapshot,
        port: u32,
    ) -> Result<(NetfilterMessage, usize, usize, bool), SystemError> {
        let interrupted = snapshot.generation != self.generation;
        let mut table_cursor = self.table_cursor;
        let mut chain_cursor = self.chain_cursor;
        while let Some(table) = snapshot.tables.get(table_cursor) {
            if self.family == 0 || self.family == table.family {
                if let Some(chain) = table.chains.get(chain_cursor) {
                    chain_cursor += 1;
                    return Ok((
                        chain_message(
                            table,
                            chain,
                            snapshot.generation,
                            self.sequence,
                            port,
                            NFT_MSG_NEWCHAIN,
                            NLM_F_MULTI | if interrupted { NLM_F_DUMP_INTR } else { 0 },
                        )?,
                        table_cursor,
                        chain_cursor,
                        false,
                    ));
                }
            }
            table_cursor += 1;
            chain_cursor = 0;
        }
        Ok((
            done_message(self.sequence, port, interrupted)?,
            table_cursor,
            chain_cursor,
            true,
        ))
    }
}

impl RuleDumpSession {
    fn next(
        &self,
        snapshot: &RulesetSnapshot,
        port: u32,
    ) -> Result<(NetfilterMessage, usize, usize, usize, bool), SystemError> {
        let interrupted = snapshot.generation != self.generation;
        let mut table_cursor = self.table_cursor;
        let mut chain_cursor = self.chain_cursor;
        let mut rule_cursor = self.rule_cursor;
        while let Some(table) = snapshot.tables.get(table_cursor) {
            let table_matches = (self.family == 0 || self.family == table.family)
                && self
                    .table_name
                    .as_ref()
                    .is_none_or(|name| table.name == *name);
            if table_matches {
                while let Some(chain) = table.chains.get(chain_cursor) {
                    if self.table_name.is_none()
                        || self
                            .chain_name
                            .as_ref()
                            .is_none_or(|name| chain.name == *name)
                    {
                        if let Some(rule) = chain.rules().get(rule_cursor) {
                            let position = rule_cursor
                                .checked_sub(1)
                                .and_then(|previous| chain.rules().get(previous))
                                .map(|previous| previous.handle);
                            rule_cursor += 1;
                            return Ok((
                                rule_message(
                                    (table, chain),
                                    (rule, position),
                                    snapshot.generation,
                                    self.sequence,
                                    port,
                                    super::NFT_MSG_NEWRULE,
                                    NLM_F_MULTI
                                        | 0x800
                                        | if interrupted { NLM_F_DUMP_INTR } else { 0 },
                                )?,
                                table_cursor,
                                chain_cursor,
                                rule_cursor,
                                false,
                            ));
                        }
                    }
                    chain_cursor += 1;
                    rule_cursor = 0;
                }
            }
            table_cursor += 1;
            chain_cursor = 0;
            rule_cursor = 0;
        }
        Ok((
            done_message(self.sequence, port, interrupted)?,
            table_cursor,
            chain_cursor,
            rule_cursor,
            true,
        ))
    }
}

impl SetDumpSession {
    fn next(
        &self,
        snapshot: &RulesetSnapshot,
        port: u32,
    ) -> Result<(NetfilterMessage, usize, usize, bool), SystemError> {
        let interrupted = snapshot.generation != self.generation;
        let mut table_cursor = self.table_cursor;
        let mut set_cursor = self.set_cursor;
        while let Some(table) = snapshot.tables.get(table_cursor) {
            if (self.family == 0 || self.family == table.family)
                && self
                    .table_name
                    .as_ref()
                    .is_none_or(|name| table.name == *name)
            {
                while let Some(set) = table.sets.get(set_cursor) {
                    set_cursor += 1;
                    if self.set_name.as_ref().is_none_or(|name| set.name == *name) {
                        return Ok((
                            set_message(
                                table,
                                set,
                                snapshot.generation,
                                self.sequence,
                                port,
                                NFT_MSG_NEWSET,
                                NLM_F_MULTI | if interrupted { NLM_F_DUMP_INTR } else { 0 },
                            )?,
                            table_cursor,
                            set_cursor,
                            false,
                        ));
                    }
                }
            }
            table_cursor += 1;
            set_cursor = 0;
        }
        Ok((
            done_message(self.sequence, port, interrupted)?,
            table_cursor,
            set_cursor,
            true,
        ))
    }
}

impl SetElementDumpSession {
    fn next(
        &self,
        snapshot: &RulesetSnapshot,
        port: u32,
    ) -> Result<(NetfilterMessage, usize, bool), SystemError> {
        let interrupted = snapshot.generation != self.generation;
        if let Some((table, set)) = snapshot
            .tables
            .iter()
            .find(|table| table.family == self.family && table.name == self.table_name)
            .and_then(|table| {
                table
                    .sets
                    .iter()
                    .find(|set| set.name == self.set_name)
                    .map(|set| (table, set))
            })
        {
            if let Some(element) = set.elements.get(self.cursor) {
                return Ok((
                    set_elem_message(
                        table,
                        set,
                        &element.key,
                        element.value.as_deref(),
                        element.verdict,
                        element.flags,
                        SetElementMessageMeta {
                            generation: snapshot.generation,
                            sequence: self.sequence,
                            port,
                            kind: NFT_MSG_NEWSETELEM,
                            flags: NLM_F_MULTI | if interrupted { NLM_F_DUMP_INTR } else { 0 },
                        },
                    )?,
                    self.cursor + 1,
                    false,
                ));
            }
        }
        Ok((
            done_message(self.sequence, port, interrupted)?,
            self.cursor,
            true,
        ))
    }
}

impl DumpSession {
    fn next(
        &self,
        snapshot: &RulesetSnapshot,
        port: u32,
    ) -> Result<(NetfilterMessage, usize, usize, usize, bool), SystemError> {
        match self {
            Self::Tables(session) => session
                .next(snapshot, port)
                .map(|(message, cursor, done)| (message, cursor, 0, 0, done)),
            Self::Chains(session) => session
                .next(snapshot, port)
                .map(|(message, table, chain, done)| (message, table, chain, 0, done)),
            Self::Rules(session) => session.next(snapshot, port),
            Self::Sets(session) => session
                .next(snapshot, port)
                .map(|(message, table, set, done)| (message, table, set, 0, done)),
            Self::SetElements(session) => session
                .next(snapshot, port)
                .map(|(message, cursor, done)| (message, 0, 0, cursor, done)),
            Self::Empty { sequence } => Ok((done_message(*sequence, port, false)?, 0, 0, 0, true)),
        }
    }

    fn advance(
        &mut self,
        table_cursor: usize,
        chain_cursor: usize,
        rule_cursor: usize,
        generation: u32,
    ) {
        match self {
            Self::Tables(session) => {
                session.cursor = table_cursor;
                session.generation = generation;
            }
            Self::Chains(session) => {
                session.table_cursor = table_cursor;
                session.chain_cursor = chain_cursor;
                session.generation = generation;
            }
            Self::Rules(session) => {
                session.table_cursor = table_cursor;
                session.chain_cursor = chain_cursor;
                session.rule_cursor = rule_cursor;
                session.generation = generation;
            }
            Self::Sets(session) => {
                session.table_cursor = table_cursor;
                session.set_cursor = chain_cursor;
                session.generation = generation;
            }
            Self::SetElements(session) => {
                session.cursor = rule_cursor;
                session.generation = generation;
            }
            Self::Empty { .. } => {}
        }
    }
}

#[derive(Default)]
struct ObjectNames<'a> {
    table: Option<&'a [u8]>,
    name: Option<&'a [u8]>,
}

fn object_names<'a>(request: &'a Request<'_>) -> Result<ObjectNames<'a>, SystemError> {
    let (table_kind, name_kind) = match request.kind() {
        NFT_MSG_GETSET | NFT_MSG_GETFLOWTABLE => (1, 2),
        _ => return Err(SystemError::EINVAL),
    };
    let mut names = ObjectNames::default();
    for nla in NlaIter::new(&request.bytes[HEADER_LEN + NFGEN_LEN..]) {
        let Nla { kind, value } = nla?;
        if kind == table_kind || kind == name_kind {
            if value.is_empty() || value.len() > 256 {
                return Err(SystemError::ERANGE);
            }
            let end = value
                .iter()
                .rposition(|byte| *byte != 0)
                .map_or(0, |index| index + 1);
            if end == 0 || end >= 256 {
                return Err(SystemError::EINVAL);
            }
            if kind == table_kind {
                names.table = Some(&value[..end]);
            } else {
                names.name = Some(&value[..end]);
            }
        } else {
            // NFNetlink applies the message policy before dispatch, even for
            // an empty dump. A short known numeric attribute is not ignored.
            let min_len = match request.kind() {
                NFT_MSG_GETSET => match kind {
                    11 | 16 => 8, // TIMEOUT, HANDLE
                    3..=8 | 10 | 12 | 15 => 4,
                    _ => 0,
                },
                NFT_MSG_GETFLOWTABLE => match kind {
                    5 => 8, // HANDLE
                    7 => 4, // FLAGS
                    _ => 0,
                },
                _ => 0,
            };
            if value.len() < min_len {
                return Err(SystemError::ERANGE);
            }
        }
    }
    Ok(names)
}

impl NftSocketState {
    pub(super) fn get_set(
        &self,
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<GetTableResult, SystemError> {
        let attrs = set_attrs(request)?;
        let family = request.bytes[HEADER_LEN];
        let snapshot = netns.nftables().snapshot();
        let sequence = u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap());
        let dump = request.flags() & NLM_F_DUMP != 0;
        if !dump && family == 0 {
            return Err(SystemError::EAFNOSUPPORT);
        }
        if attrs.table.is_some_and(|name| {
            !snapshot
                .tables
                .iter()
                .any(|table| table.family == family && table.name == name)
        }) {
            return Err(SystemError::ENOENT);
        }
        if !dump {
            let table_name = attrs.table.ok_or(SystemError::EINVAL)?;
            let table = snapshot
                .tables
                .iter()
                .find(|table| table.family == family && table.name == table_name)
                .ok_or(SystemError::ENOENT)?;
            let name = attrs.name.ok_or(SystemError::EINVAL)?;
            let set = table
                .sets
                .iter()
                .find(|set| set.name == name)
                .ok_or(SystemError::ENOENT)?;
            NetlinkNetfilterProtocol::unicast(
                port,
                set_message(
                    table,
                    set,
                    snapshot.generation,
                    sequence,
                    port,
                    NFT_MSG_NEWSET,
                    0,
                )?,
                netns.clone(),
            )?;
            return Ok(GetTableResult::Replied);
        }
        let mut slot = self.dump.lock();
        if slot.is_some() {
            return Err(SystemError::EBUSY);
        }
        *slot = Some(DumpSession::Sets(SetDumpSession {
            generation: snapshot.generation,
            family,
            sequence,
            table_name: own_optional_name(attrs.table)?,
            set_name: own_optional_name(attrs.name)?,
            table_cursor: 0,
            set_cursor: 0,
        }));
        if let Err(error) = Self::drive_locked(&mut slot, port, netns) {
            *slot = None;
            return Err(error);
        }
        Ok(GetTableResult::DumpStarted)
    }

    pub(super) fn get_set_elements(
        &self,
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<GetTableResult, SystemError> {
        let attrs = set_elem_attrs(request)?;
        let family = request.bytes[HEADER_LEN];
        let table_name = attrs.table.ok_or(SystemError::EINVAL)?;
        let set_name = attrs.set.ok_or(SystemError::EINVAL)?;
        let snapshot = netns.nftables().snapshot();
        let table = snapshot
            .tables
            .iter()
            .find(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let set = table
            .sets
            .iter()
            .find(|set| set.name == set_name)
            .ok_or(SystemError::ENOENT)?;
        let sequence = u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap());
        if request.flags() & NLM_F_DUMP == 0 {
            if attrs.elements.len() != 1 {
                return Err(SystemError::EINVAL);
            }
            let key = attrs.elements[0].key;
            let index = set
                .get_element_index(key, attrs.elements[0].flags)
                .ok_or(SystemError::ENOENT)?;
            let element = &set.elements[index];
            NetlinkNetfilterProtocol::unicast(
                port,
                set_elem_message(
                    table,
                    set,
                    &element.key,
                    element.value.as_deref(),
                    element.verdict,
                    element.flags,
                    SetElementMessageMeta {
                        generation: snapshot.generation,
                        sequence,
                        port,
                        kind: NFT_MSG_NEWSETELEM,
                        flags: 0,
                    },
                )?,
                netns.clone(),
            )?;
            return Ok(GetTableResult::Replied);
        }
        if !attrs.elements.is_empty() || attrs.set_id.is_some() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let mut slot = self.dump.lock();
        if slot.is_some() {
            return Err(SystemError::EBUSY);
        }
        *slot = Some(DumpSession::SetElements(SetElementDumpSession {
            generation: snapshot.generation,
            family,
            sequence,
            table_name: own_optional_name(Some(table_name))?.unwrap(),
            set_name: own_optional_name(Some(set_name))?.unwrap(),
            cursor: 0,
        }));
        if let Err(error) = Self::drive_locked(&mut slot, port, netns) {
            *slot = None;
            return Err(error);
        }
        Ok(GetTableResult::DumpStarted)
    }

    pub(super) fn get_rule(
        &self,
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<GetTableResult, SystemError> {
        let attrs = rule_attrs(request)?;
        let family = request.bytes[HEADER_LEN];
        let snapshot = netns.nftables().snapshot();
        if request.flags() & NLM_F_DUMP == 0 {
            let table_name = attrs.table.ok_or(SystemError::EINVAL)?;
            let table = snapshot
                .tables
                .iter()
                .find(|table| table.family == family && table.name == table_name)
                .ok_or(SystemError::ENOENT)?;
            let chain_name = attrs.chain.ok_or(SystemError::EINVAL)?;
            let chain = table
                .chains
                .iter()
                .find(|chain| chain.name == chain_name)
                .ok_or(SystemError::ENOENT)?;
            let handle = attrs.handle.ok_or(SystemError::EINVAL)?;
            let rule = chain
                .rules()
                .iter()
                .find(|rule| rule.handle == handle)
                .ok_or(SystemError::ENOENT)?;
            let sequence = u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap());
            let message = rule_message(
                (table, chain),
                (rule, None),
                snapshot.generation,
                sequence,
                port,
                super::NFT_MSG_NEWRULE,
                0,
            )?;
            NetlinkNetfilterProtocol::unicast(port, message, netns.clone())?;
            return Ok(GetTableResult::Replied);
        }
        let mut slot = self.dump.lock();
        if slot.is_some() {
            return Err(SystemError::EBUSY);
        }
        *slot = Some(DumpSession::Rules(RuleDumpSession {
            generation: snapshot.generation,
            family,
            sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
            table_name: own_optional_name(attrs.table)?,
            chain_name: own_optional_name(attrs.chain)?,
            table_cursor: 0,
            chain_cursor: 0,
            rule_cursor: 0,
        }));
        if let Err(error) = Self::drive_locked(&mut slot, port, netns) {
            *slot = None;
            return Err(error);
        }
        Ok(GetTableResult::DumpStarted)
    }

    pub(super) fn get_chain(
        &self,
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<GetTableResult, SystemError> {
        let attrs = chain_attrs(request)?;
        let family = request.bytes[HEADER_LEN];
        let snapshot = netns.nftables().snapshot();
        if request.flags() & NLM_F_DUMP == 0 {
            let table_name = attrs.table.ok_or(SystemError::EINVAL)?;
            let table = snapshot
                .tables
                .iter()
                .find(|table| table.family == family && table.name == table_name)
                .ok_or(SystemError::ENOENT)?;
            let chain_name = attrs.name.ok_or(SystemError::EINVAL)?;
            let chain = table
                .chains
                .iter()
                .find(|chain| chain.name == chain_name)
                .ok_or(SystemError::ENOENT)?;
            let seq = u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap());
            let message = chain_message(
                table,
                chain,
                snapshot.generation,
                seq,
                port,
                NFT_MSG_NEWCHAIN,
                0,
            )?;
            NetlinkNetfilterProtocol::unicast(port, message, netns.clone())?;
            return Ok(GetTableResult::Replied);
        }
        let mut slot = self.dump.lock();
        if slot.is_some() {
            return Err(SystemError::EBUSY);
        }
        *slot = Some(DumpSession::Chains(ChainDumpSession {
            generation: snapshot.generation,
            family,
            sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
            table_cursor: 0,
            chain_cursor: 0,
        }));
        if let Err(error) = Self::drive_locked(&mut slot, port, netns) {
            *slot = None;
            return Err(error);
        }
        Ok(GetTableResult::DumpStarted)
    }

    pub(super) fn get_table(
        &self,
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<GetTableResult, SystemError> {
        let attrs = table_attrs(request)?;
        let family = request.bytes[HEADER_LEN];
        let snapshot = netns.nftables().snapshot();
        if request.flags() & NLM_F_DUMP == 0 {
            let name = attrs.name.ok_or(SystemError::EINVAL)?;
            let table = snapshot
                .tables
                .iter()
                .find(|table| table.family == family && table.name == name)
                .ok_or(SystemError::ENOENT)?;
            let seq = u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap());
            let message = table_message(
                table,
                snapshot.generation,
                seq,
                port,
                NFT_MSG_NEWTABLE,
                0,
                None,
            )?;
            NetlinkNetfilterProtocol::unicast(port, message, netns.clone())?;
            return Ok(GetTableResult::Replied);
        }

        let mut slot = self.dump.lock();
        if slot.is_some() {
            return Err(SystemError::EBUSY);
        }
        *slot = Some(DumpSession::Tables(TableDumpSession {
            generation: snapshot.generation,
            family,
            sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
            cursor: 0,
        }));
        if let Err(error) = Self::drive_locked(&mut slot, port, netns) {
            *slot = None;
            return Err(error);
        }
        Ok(GetTableResult::DumpStarted)
    }

    /// Read the genuinely empty set/flowtable classes. No corresponding
    /// NEW operation may succeed until its validated execution path exists.
    pub(super) fn get_empty_object(
        &self,
        request: &Request<'_>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<GetTableResult, SystemError> {
        let names = object_names(request)?;
        let family = request.bytes[HEADER_LEN];
        let dump = request.flags() & NLM_F_DUMP != 0;
        let table_exists = || {
            names.table.is_some_and(|name| {
                netns
                    .nftables()
                    .snapshot()
                    .tables
                    .iter()
                    .any(|table| table.family == family && table.name == name)
            })
        };
        match request.kind() {
            NFT_MSG_GETSET => {
                if names.table.is_some() && !table_exists() {
                    return Err(SystemError::ENOENT);
                }
                if !dump {
                    if family == 0 {
                        return Err(SystemError::EAFNOSUPPORT);
                    }
                    names.table.ok_or(SystemError::EINVAL)?;
                    names.name.ok_or(SystemError::EINVAL)?;
                    return Err(SystemError::ENOENT);
                }
            }
            NFT_MSG_GETFLOWTABLE if !dump => {
                names.name.ok_or(SystemError::EINVAL)?;
                names.table.ok_or(SystemError::EINVAL)?;
                return Err(SystemError::ENOENT);
            }
            _ => {}
        }
        let mut slot = self.dump.lock();
        if slot.is_some() {
            return Err(SystemError::EBUSY);
        }
        *slot = Some(DumpSession::Empty {
            sequence: u32::from_ne_bytes(request.bytes[8..12].try_into().unwrap()),
        });
        if let Err(error) = Self::drive_locked(&mut slot, port, netns) {
            *slot = None;
            return Err(error);
        }
        Ok(GetTableResult::DumpStarted)
    }

    pub(super) fn drive_if_queue_empty(
        &self,
        queue: &MessageQueue<NetfilterMessage>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) {
        let mut slot = self.dump.lock();
        if !queue.0.lock().is_empty() {
            return;
        }
        if Self::drive_locked(&mut slot, port, netns).is_err() {
            // The caller may already have consumed a page. Report a lost
            // continuation to userspace; never leave a dump waiting forever.
            *slot = None;
            NetlinkNetfilterProtocol::report_overrun(port, netns.clone());
        }
    }

    fn drive_locked(
        slot: &mut Option<DumpSession>,
        port: u32,
        netns: &Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        let Some(session) = slot.as_mut() else {
            return Ok(());
        };
        let snapshot = netns.nftables().snapshot();
        let (message, table_cursor, chain_cursor, rule_cursor, done) =
            session.next(&snapshot, port)?;
        NetlinkNetfilterProtocol::unicast(port, message, netns.clone())?;
        session.advance(table_cursor, chain_cursor, rule_cursor, snapshot.generation);
        // Match nl_dump_check_consistent(): advertise a generation transition
        // once, then use that generation as the baseline for the next page.
        if done {
            *slot = None;
        }
        Ok(())
    }
}
