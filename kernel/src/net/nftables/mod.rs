//! Network-namespace-owned nftables ruleset state.
//!
//! The initial snapshot is genuinely empty. Table, chain, and rule objects
//! will be added here together with their validated execution paths; a
//! netlink write must never report success for an object the packet path
//! cannot enforce.

use crate::{
    libs::{
        mutex::{Mutex, MutexGuard},
        spinlock::SpinLock,
    },
    mm::percpu::PerCpu,
    process::namespace::net_namespace::NetNamespace,
    rcu::{PreparedRcuArcRetire, RcuArcSlot},
    smp::core::smp_get_processor_id,
};
use alloc::{sync::Arc, vec::Vec};
use core::cell::{Cell, RefCell};
use smoltcp::wire::{IpVersion, Ipv4Address, Ipv6Address};
use system_error::SystemError;

use super::conntrack::{
    CtAddress, CtError, CtNatPortRange, CtNatRequest, CtPacketContext, CtPacketState, CtState,
    NatManipSide,
};

mod hook;
mod objects;
mod packet;
mod rule;
#[cfg(test)]
mod tests;
mod transaction;

pub(crate) use hook::RulesetSnapshot;
pub(crate) use objects::{
    CreatedChain, CreatedRule, NftBaseChain, NftChain, NftChainType, NftIpv4Hook, NftSet,
    NftSetElement, NftSetElementInput, NftTable,
};
pub(crate) use packet::{NftDeviceNames, NftNatPacket, NftPacket};
pub(crate) use rule::{
    NftBitwiseOperation, NftByteorderOp, NftCmpOp, NftCounter, NftExpression, NftExpressionInput,
    NftMetaKey, NftNatAction, NftNatEvent, NftNatProgress, NftPayloadBase, NftRangeOp, NftRule,
    NftRuleInput, NftRuleVerdict, NftVerdict, NftXtAddrtype, NftXtConntrack, NftXtTcp,
};
pub(crate) use transaction::{NewSetSpec, NftNamespaceState, NftTransaction};

use hook::HookProgram;
use objects::{allocated_set_name, copy_bytes};
use rule::{data_register, parse_xt_nat_target, ChainResult, RuleResult};
