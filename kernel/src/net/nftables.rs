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

type RedirectAddressResolver<'a> = dyn Fn(&[u8]) -> Option<CtAddress> + 'a;

/// The packet path supplies its already-acquired routing view. Route-aware
/// expressions must not acquire the FIB behind a smoltcp interface lock.
pub(crate) struct NftPacket<'a> {
    bytes: &'a [u8],
    conntrack: Option<&'a RefCell<Option<CtPacketContext>>>,
    mark: Option<&'a Cell<u32>>,
    redirect_address: Option<&'a RedirectAddressResolver<'a>>,
    ipv4_addr_type: Option<&'a dyn Fn(Ipv4Address) -> u8>,
    ipv6_local_destination: Option<&'a dyn Fn(Ipv6Address) -> bool>,
    iifname: [u8; 16],
    oifname: [u8; 16],
}

/// Mutable packet view for a hook that may run NAT. The VM reconstructs a
/// short-lived read-only NftPacket after each rewrite, so later chains see
/// the translated headers without holding an alias across the callback.
pub(crate) struct NftNatPacket<'a> {
    bytes: &'a mut [u8],
    conntrack: &'a RefCell<Option<CtPacketContext>>,
    mark: Option<&'a Cell<u32>>,
    redirect_address: Option<&'a RedirectAddressResolver<'a>>,
    iifname: [u8; 16],
    oifname: [u8; 16],
    ipv4_addr_type: Option<&'a dyn Fn(Ipv4Address) -> u8>,
    ipv6_local_destination: Option<&'a dyn Fn(Ipv6Address) -> bool>,
}

impl<'a> NftNatPacket<'a> {
    pub(crate) fn new_ipv4(
        bytes: &'a mut [u8],
        conntrack: &'a RefCell<Option<CtPacketContext>>,
        iifname: [u8; 16],
        oifname: [u8; 16],
        addr_type: &'a dyn Fn(Ipv4Address) -> u8,
    ) -> Self {
        Self {
            bytes,
            conntrack,
            mark: None,
            redirect_address: None,
            iifname,
            oifname,
            ipv4_addr_type: Some(addr_type),
            ipv6_local_destination: None,
        }
    }

    pub(crate) fn new_ipv6(
        bytes: &'a mut [u8],
        conntrack: &'a RefCell<Option<CtPacketContext>>,
        iifname: [u8; 16],
        oifname: [u8; 16],
    ) -> Self {
        Self {
            bytes,
            conntrack,
            mark: None,
            redirect_address: None,
            iifname,
            oifname,
            ipv4_addr_type: None,
            ipv6_local_destination: None,
        }
    }

    pub(crate) fn with_ipv6_local_destination(
        mut self,
        lookup: &'a dyn Fn(Ipv6Address) -> bool,
    ) -> Self {
        self.ipv6_local_destination = Some(lookup);
        self
    }

    pub(crate) fn with_mark(mut self, mark: &'a Cell<u32>) -> Self {
        self.mark = Some(mark);
        self
    }

    pub(crate) fn with_redirect_address(
        mut self,
        resolve: &'a dyn Fn(&[u8]) -> Option<CtAddress>,
    ) -> Self {
        self.redirect_address = Some(resolve);
        self
    }
}

impl<'a> NftPacket<'a> {
    pub(crate) fn new(bytes: &'a [u8], ipv4_addr_type: &'a dyn Fn(Ipv4Address) -> u8) -> Self {
        Self {
            bytes,
            conntrack: None,
            mark: None,
            redirect_address: None,
            ipv4_addr_type: Some(ipv4_addr_type),
            ipv6_local_destination: None,
            iifname: [0; 16],
            oifname: [0; 16],
        }
    }

    pub(crate) fn new_ipv6(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            conntrack: None,
            mark: None,
            redirect_address: None,
            ipv4_addr_type: None,
            ipv6_local_destination: None,
            iifname: [0; 16],
            oifname: [0; 16],
        }
    }

    pub(crate) fn with_ipv6_local_destination(
        mut self,
        lookup: &'a dyn Fn(Ipv6Address) -> bool,
    ) -> Self {
        self.ipv6_local_destination = Some(lookup);
        self
    }

    pub(crate) fn with_interface_names(mut self, iifname: [u8; 16], oifname: [u8; 16]) -> Self {
        self.iifname = iifname;
        self.oifname = oifname;
        self
    }

    pub(crate) fn with_conntrack_cell(
        mut self,
        context: &'a RefCell<Option<CtPacketContext>>,
    ) -> Self {
        self.conntrack = Some(context);
        self
    }

    pub(crate) fn with_mark(mut self, mark: &'a Cell<u32>) -> Self {
        self.mark = Some(mark);
        self
    }

    pub(crate) fn with_redirect_address(
        mut self,
        resolve: &'a dyn Fn(&[u8]) -> Option<CtAddress>,
    ) -> Self {
        self.redirect_address = Some(resolve);
        self
    }

    fn ct_state_bits(&self) -> u32 {
        let context = self.conntrack.as_ref().map(|cell| cell.borrow());
        match context.as_deref().and_then(Option::as_ref) {
            Some(CtPacketContext::Untracked) => 1 << 6,
            Some(context) => match context.state() {
                Some(CtPacketState::Established) => 1 << 1,
                Some(CtPacketState::Related) => 1 << 2,
                Some(CtPacketState::New) => 1 << 3,
                Some(CtPacketState::Invalid) | None => 1,
            },
            // Before the -200 tracking hook there is no skb conntrack
            // identity. Linux's native ct state expression reads INVALID.
            None => 1,
        }
    }
}

/// Names are copied before the poller acquires FIB or smoltcp locks. A rename
/// can publish a new name later; this poll evaluates against one prior view.
pub(crate) struct NftDeviceNames(Vec<(u32, [u8; 16])>);

impl NftDeviceNames {
    pub(crate) fn empty() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn snapshot(netns: &NetNamespace) -> Result<Self, SystemError> {
        let devices = netns.device_list();
        let mut names = Vec::new();
        names
            .try_reserve_exact(devices.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for (ifindex, iface) in devices.iter() {
            let ifindex = u32::try_from(*ifindex).map_err(|_| SystemError::ERANGE)?;
            let mut name = [0; 16];
            iface.common().with_iface_name(|value| {
                let bytes = value.as_bytes();
                name[..bytes.len().min(15)].copy_from_slice(&bytes[..bytes.len().min(15)]);
            });
            names.push((ifindex, name));
        }
        Ok(Self(names))
    }

    /// A missing hook device is represented by index zero. A nonzero index
    /// absent from this poll's snapshot is *not* the same thing: evaluating
    /// its name as an empty string could admit a packet through a false match.
    pub(crate) fn get(&self, ifindex: u32) -> Option<[u8; 16]> {
        if ifindex == 0 {
            return Some([0; 16]);
        }
        self.0
            .binary_search_by_key(&ifindex, |(index, _)| *index)
            .ok()
            .map(|index| self.0[index].1)
    }
}

#[cfg(test)]
mod device_name_tests {
    use super::NftDeviceNames;

    #[test]
    fn absent_device_is_distinct_from_a_hook_without_a_device() {
        let mut loopback = [0; 16];
        loopback[..2].copy_from_slice(b"lo");
        let names = NftDeviceNames(alloc::vec![(1, loopback)]);
        assert_eq!(names.get(0), Some([0; 16]));
        assert_eq!(names.get(1), Some(loopback));
        assert_eq!(names.get(2), None);
    }
}

#[cfg(test)]
mod ct_state_tests {
    use super::*;
    use crate::net::conntrack::{CtCandidate, CtL4, CtPacketKind, CtTuple};

    #[test]
    fn native_ct_state_distinguishes_untracked_from_invalid_and_new() {
        let packet = [0u8; 20];
        let addr_type = |_| 0;
        let untracked = RefCell::new(Some(CtPacketContext::Untracked));
        let invalid = RefCell::new(Some(CtPacketContext::Invalid));
        let candidate = RefCell::new(Some(CtPacketContext::Candidate(
            CtCandidate::new(
                CtTuple {
                    src: [192, 0, 2, 1].into(),
                    dst: [198, 51, 100, 1].into(),
                    l4: CtL4::Udp {
                        src_port: 10000,
                        dst_port: 53,
                    },
                },
                CtPacketKind::Udp,
            )
            .unwrap(),
        )));
        let base = NftPacket::new(&packet, &addr_type);
        assert_eq!(base.ct_state_bits(), 1);
        assert_eq!(base.with_conntrack_cell(&untracked).ct_state_bits(), 64);
        assert_eq!(
            NftPacket::new(&packet, &addr_type)
                .with_conntrack_cell(&invalid)
                .ct_state_bits(),
            1
        );
        assert_eq!(
            NftPacket::new(&packet, &addr_type)
                .with_conntrack_cell(&candidate)
                .ct_state_bits(),
            8
        );
    }

    #[test]
    fn native_ct_state_register_drives_rule_comparison() {
        let rule = NftRule {
            handle: 1,
            expressions: alloc::vec![
                NftExpression::CtState { dreg: 0 },
                NftExpression::Cmp {
                    sreg: 0,
                    op: NftCmpOp::Eq,
                    data: 8u32.to_ne_bytes().to_vec(),
                },
                NftExpression::Immediate(NftRuleVerdict::Accept),
            ],
        };
        let packet = [0u8; 20];
        let addr_type = |_| 0;
        let new = RefCell::new(Some(CtPacketContext::Candidate(
            CtCandidate::new(
                CtTuple {
                    src: [192, 0, 2, 1].into(),
                    dst: [198, 51, 100, 1].into(),
                    l4: CtL4::Udp {
                        src_port: 10000,
                        dst_port: 53,
                    },
                },
                CtPacketKind::Udp,
            )
            .unwrap(),
        )));
        assert_eq!(
            rule.evaluate(
                &NftPacket::new(&packet, &addr_type).with_conntrack_cell(&new),
                &[]
            ),
            RuleResult::Verdict(NftRuleVerdict::Accept)
        );
        assert_eq!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type), &[]),
            RuleResult::Break
        );
    }

    #[test]
    fn xt_conntrack_accepts_only_executable_state_predicates() {
        let mut info = [0u8; 160];
        info[146..148].copy_from_slice(&1u16.to_ne_bytes());
        info[150..152].copy_from_slice(&8u16.to_ne_bytes());
        let matcher = NftXtConntrack::from_info(2, &info).unwrap();
        let packet = [0u8; 20];
        let addr_type = |_| 0;
        assert!(!matcher.evaluate(&NftPacket::new(&packet, &addr_type)));
        info[150..152].copy_from_slice(&512u16.to_ne_bytes());
        let matcher = NftXtConntrack::from_info(2, &info).unwrap();
        let untracked = RefCell::new(Some(CtPacketContext::Untracked));
        assert!(
            matcher.evaluate(&NftPacket::new(&packet, &addr_type).with_conntrack_cell(&untracked))
        );
        info[152..154].copy_from_slice(&1u16.to_ne_bytes());
        assert!(matches!(
            NftXtConntrack::from_info(2, &info),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
        info[152..154].fill(0);
        info[150..152].copy_from_slice(&128u16.to_ne_bytes());
        assert!(matches!(
            NftXtConntrack::from_info(2, &info),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
    }

    #[test]
    fn ct_state_rule_is_compiled_but_other_ct_keys_are_rejected() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(2, b"filter", b"input", None, false, false)
            .unwrap();
        let wanted = 8u32.to_ne_bytes();
        let expressions = [
            NftExpressionInput::Ct {
                key: 0,
                dreg: 8,
                direction: None,
            },
            NftExpressionInput::Cmp {
                sreg: 8,
                op: 0,
                data: &wanted,
            },
            NftExpressionInput::Immediate(NftRuleInput::Accept),
        ];
        let (_, _, rule) = transaction
            .new_ip_rule(2, b"filter", b"input", &expressions, true, None)
            .unwrap();
        assert!(matches!(
            rule.expressions()[0],
            NftExpression::CtState { dreg: 0 }
        ));
        let unsupported = [NftExpressionInput::Ct {
            key: 2,
            dreg: 8,
            direction: None,
        }];
        assert!(matches!(
            transaction.new_ip_rule(2, b"filter", b"input", &unsupported, true, None),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
        let invalid = [NftExpressionInput::Ct {
            key: 0,
            dreg: 8,
            direction: Some(0),
        }];
        assert!(matches!(
            transaction.new_ip_rule(2, b"filter", b"input", &invalid, true, None),
            Err(SystemError::EINVAL)
        ));
    }

    #[test]
    fn immediate_value_initializes_nat_address_and_port_registers() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"nat", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(
                2,
                b"nat",
                b"output",
                Some((
                    NftIpv4Hook::LocalOut,
                    -100,
                    NftVerdict::Accept,
                    NftChainType::Nat,
                )),
                false,
                false,
            )
            .unwrap();
        let address = [127u8, 0, 0, 1];
        let port = 5001u16.to_be_bytes();
        let expressions = [
            NftExpressionInput::ImmediateData {
                dreg: 1,
                data: &address,
            },
            NftExpressionInput::ImmediateData {
                dreg: 2,
                data: &port,
            },
            NftExpressionInput::Nat {
                nat_type: 1,
                family: 2,
                addr_min_reg: Some(1),
                addr_max_reg: None,
                proto_min_reg: Some(2),
                proto_max_reg: None,
                flags: 0,
            },
        ];
        let (_, _, rule) = transaction
            .new_ip_rule(2, b"nat", b"output", &expressions, true, None)
            .unwrap();
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type), &[]),
            RuleResult::Nat(NftNatAction::Dnat(_))
        ));
        let unsupported = [NftExpressionInput::Nat {
            nat_type: 1,
            family: 2,
            addr_min_reg: Some(1),
            addr_max_reg: None,
            proto_min_reg: None,
            proto_max_reg: None,
            flags: 0,
        }];
        assert!(matches!(
            transaction.new_ip_rule(2, b"nat", b"output", &unsupported, true, None),
            Err(SystemError::ENODATA)
        ));
    }

    #[test]
    fn ct_registration_order_is_reserved_at_rule_creation() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        let hook = Some((
            NftIpv4Hook::PreRouting,
            -200,
            NftVerdict::Accept,
            NftChainType::Filter,
        ));
        let (_, older) = transaction
            .new_ip_chain(2, b"filter", b"older", hook, false, false)
            .unwrap()
            .unwrap();
        let ct = [NftExpressionInput::Ct {
            key: 0,
            dreg: 8,
            direction: None,
        }];
        transaction
            .new_ip_rule(2, b"filter", b"older", &ct, true, None)
            .unwrap();
        let registration = transaction.pending_ct_hook_order[0].unwrap();
        let (_, newer) = transaction
            .new_ip_chain(2, b"filter", b"newer", hook, false, false)
            .unwrap()
            .unwrap();
        assert!(older.base.unwrap().hook_order < registration);
        assert!(registration < newer.base.unwrap().hook_order);
        assert_eq!(transaction.pending_ct_hook_order[1], None);
    }

    #[test]
    fn exact_set_id_lookup_matches_elements_and_blocks_referenced_delete() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(2, b"filter", b"input", None, false, false)
            .unwrap();
        let (_, set) = transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"addresses",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: None,
                userdata: &[],
                flags: 0,
                id: Some(13),
                exclusive: true,
            })
            .unwrap()
            .unwrap();
        assert!(set.elements.is_empty());
        let (_, set) = transaction
            .update_set_elements(
                2,
                b"filter",
                b"addresses",
                &[NftSetElementInput {
                    key: &[127, 0, 0, 1],
                    value: None,
                    verdict: None,
                    flags: 0,
                    key_end: None,
                }],
                true,
            )
            .unwrap();
        let (_, later) = transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"later",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: None,
                userdata: &[],
                flags: 0,
                id: None,
                exclusive: true,
            })
            .unwrap()
            .unwrap();
        assert!(set.handle < later.handle);
        transaction.del_set(2, b"filter", b"later").unwrap();
        let (_, newest) = transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"newest",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: None,
                userdata: &[],
                flags: 0,
                id: None,
                exclusive: true,
            })
            .unwrap()
            .unwrap();
        assert!(later.handle < newest.handle);
        assert!(transaction.candidate.tables[0]
            .sets
            .windows(2)
            .all(|pair| pair[0].handle < pair[1].handle));
        let key = [127, 0, 0, 1];
        let expressions = [
            NftExpressionInput::ImmediateData {
                dreg: 8,
                data: &key,
            },
            NftExpressionInput::Lookup {
                set: Some(b"addresses"),
                set_id: Some(13),
                sreg: 8,
                dreg: None,
                invert: false,
            },
            NftExpressionInput::Immediate(NftRuleInput::Drop),
        ];
        let (_, _, rule) = transaction
            .new_ip_rule(2, b"filter", b"input", &expressions, true, None)
            .unwrap();
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type), &[set]),
            RuleResult::Verdict(NftRuleVerdict::Drop)
        ));
        assert!(matches!(
            transaction.del_set(2, b"filter", b"addresses"),
            Err(SystemError::EBUSY)
        ));
        drop(transaction);
        assert!(state.snapshot().tables.is_empty());
    }

    #[test]
    fn verdict_map_lookup_executes_and_missing_key_breaks_rule() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(2, b"filter", b"input", None, false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"decision",
                key_type: 7,
                key_len: 4,
                data_type: Some(0xffff_ff00),
                data_len: Some(0),
                size: None,
                userdata: &[],
                flags: 8,
                id: Some(5),
                exclusive: true,
            })
            .unwrap();
        let (_, set) = transaction
            .update_set_elements(
                2,
                b"filter",
                b"decision",
                &[NftSetElementInput {
                    key: &[127, 0, 0, 1],
                    value: None,
                    verdict: Some(NftRuleInput::Drop),
                    flags: 0,
                    key_end: None,
                }],
                true,
            )
            .unwrap();
        let key = [127, 0, 0, 1];
        let expressions = [
            NftExpressionInput::ImmediateData {
                dreg: 8,
                data: &key,
            },
            NftExpressionInput::Lookup {
                set: Some(b"decision"),
                set_id: Some(5),
                sreg: 8,
                dreg: Some(0),
                invert: false,
            },
        ];
        let (_, _, rule) = transaction
            .new_ip_rule(2, b"filter", b"input", &expressions, true, None)
            .unwrap();
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type), &[set]),
            RuleResult::Verdict(NftRuleVerdict::Drop)
        ));
        assert!(matches!(
            transaction.del_set(2, b"filter", b"decision"),
            Err(SystemError::EBUSY)
        ));
    }

    #[test]
    fn verdict_map_jump_uses_chain_graph_and_blocks_target_delete() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(
                2,
                b"filter",
                b"input",
                Some((
                    NftIpv4Hook::LocalIn,
                    0,
                    NftVerdict::Accept,
                    NftChainType::Filter,
                )),
                false,
                false,
            )
            .unwrap();
        transaction
            .new_ip_chain(2, b"filter", b"target", None, false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"decision",
                key_type: 7,
                key_len: 4,
                data_type: Some(0xffff_ff00),
                data_len: Some(0),
                size: None,
                userdata: &[],
                flags: 8,
                id: None,
                exclusive: true,
            })
            .unwrap();
        transaction
            .update_set_elements(
                2,
                b"filter",
                b"decision",
                &[NftSetElementInput {
                    key: &[127, 0, 0, 1],
                    value: None,
                    verdict: Some(NftRuleInput::Jump(b"target")),
                    flags: 0,
                    key_end: None,
                }],
                true,
            )
            .unwrap();
        assert!(matches!(
            transaction.del_chain(2, b"filter", Some(b"target"), None, false),
            Err(SystemError::EBUSY)
        ));
        transaction
            .new_ip_rule(
                2,
                b"filter",
                b"target",
                &[NftExpressionInput::Immediate(NftRuleInput::Drop)],
                true,
                None,
            )
            .unwrap();
        let key = [127, 0, 0, 1];
        transaction
            .new_ip_rule(
                2,
                b"filter",
                b"input",
                &[
                    NftExpressionInput::ImmediateData {
                        dreg: 8,
                        data: &key,
                    },
                    NftExpressionInput::Lookup {
                        set: Some(b"decision"),
                        set_id: None,
                        sreg: 8,
                        dreg: Some(0),
                        invert: false,
                    },
                ],
                true,
                None,
            )
            .unwrap();
        let table = &transaction.candidate.tables[0];
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        assert!(matches!(
            table.evaluate_base_chain_result(
                table.chains[0].handle,
                &NftPacket::new(&packet, &addr_type),
            ),
            ChainResult::Verdict(NftVerdict::Drop)
        ));
    }

    #[test]
    fn interval_boundaries_match_inside_and_not_at_end() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(2, b"filter", b"input", None, false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"subnet",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: None,
                userdata: &[],
                flags: 4,
                id: None,
                exclusive: true,
            })
            .unwrap();
        let boundaries = [
            NftSetElementInput {
                key: &[0, 0, 0, 0],
                value: None,
                verdict: None,
                flags: 1,
                key_end: None,
            },
            NftSetElementInput {
                key: &[127, 0, 0, 0],
                value: None,
                verdict: None,
                flags: 0,
                key_end: None,
            },
            NftSetElementInput {
                key: &[128, 0, 0, 0],
                value: None,
                verdict: None,
                flags: 1,
                key_end: None,
            },
        ];
        let (_, set) = transaction
            .update_set_elements(2, b"filter", b"subnet", &boundaries, true)
            .unwrap();
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        for (key, expected_match) in [
            ([126, 255, 255, 255], false),
            ([127, 0, 0, 0], true),
            ([127, 0, 0, 1], true),
            ([128, 0, 0, 0], false),
        ] {
            let expressions = [
                NftExpressionInput::ImmediateData {
                    dreg: 8,
                    data: &key,
                },
                NftExpressionInput::Lookup {
                    set: Some(b"subnet"),
                    set_id: None,
                    sreg: 8,
                    dreg: None,
                    invert: false,
                },
                NftExpressionInput::Immediate(NftRuleInput::Drop),
            ];
            let (_, _, rule) = transaction
                .new_ip_rule(2, b"filter", b"input", &expressions, true, None)
                .unwrap();
            assert_eq!(
                matches!(
                    rule.evaluate(&NftPacket::new(&packet, &addr_type), &[set.clone()]),
                    RuleResult::Verdict(NftRuleVerdict::Drop)
                ),
                expected_match
            );
        }
        let original = Arc::as_ptr(&transaction.candidate.tables[0].sets[0]);
        assert!(matches!(
            transaction.update_set_elements(
                2,
                b"filter",
                b"subnet",
                &[NftSetElementInput {
                    key: &[127, 0, 1, 0],
                    value: None,
                    verdict: None,
                    flags: 0,
                    key_end: None,
                }],
                true,
            ),
            Err(SystemError::EINVAL)
        ));
        assert!(matches!(
            transaction.update_set_elements(
                2,
                b"filter",
                b"subnet",
                &[NftSetElementInput {
                    key: &[127, 0, 0, 0],
                    value: None,
                    verdict: None,
                    flags: 0,
                    key_end: Some(&[0, 0, 0, 0]),
                }],
                false,
            ),
            Err(SystemError::EINVAL)
        ));
        assert_eq!(
            original,
            Arc::as_ptr(&transaction.candidate.tables[0].sets[0])
        );
        assert_eq!(transaction.candidate.tables[0].sets[0].elements.len(), 3);
        let range = [NftSetElementInput {
            key: &[127, 0, 0, 0],
            value: None,
            verdict: None,
            flags: 0,
            key_end: Some(&[128, 0, 0, 0]),
        }];
        let (_, updated) = transaction
            .update_set_elements(2, b"filter", b"subnet", &range, false)
            .unwrap();
        assert_eq!(updated.elements.len(), 1);
        assert_eq!(updated.elements[0].key, [0, 0, 0, 0]);
    }

    #[test]
    fn adjacent_intervals_share_end_and_start_key_without_overlap() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"subnet",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: None,
                userdata: &[],
                flags: 4,
                id: None,
                exclusive: true,
            })
            .unwrap();
        let boundaries = [
            ([0, 0, 0, 0], 1),
            ([10, 0, 0, 0], 0),
            ([11, 0, 0, 0], 1),
            ([11, 0, 0, 0], 0),
            ([12, 0, 0, 0], 1),
        ];
        let inputs = boundaries
            .each_ref()
            .map(|(key, flags)| NftSetElementInput {
                key,
                value: None,
                verdict: None,
                flags: *flags,
                key_end: None,
            });
        transaction
            .update_set_elements(2, b"filter", b"subnet", &inputs, true)
            .unwrap();
        let set = &transaction.candidate.tables[0].sets[0];
        assert_eq!(set.element_index(&[11, 0, 0, 0], 1), Some(2));
        assert_eq!(set.element_index(&[11, 0, 0, 0], 0), Some(3));
        assert_eq!(set.get_element_index(&[10, 1, 0, 1], 0), Some(1));
        assert_eq!(set.get_element_index(&[10, 1, 0, 1], 1), Some(2));
        assert_eq!(set.get_element_index(&[11, 0, 0, 0], 0), Some(3));
        assert_eq!(set.get_element_index(&[11, 0, 0, 0], 1), Some(2));
        assert_eq!(set.get_element_index(&[11, 1, 0, 1], 0), Some(3));
        assert_eq!(set.get_element_index(&[11, 1, 0, 1], 1), Some(4));
        assert_eq!(set.get_element_index(&[12, 0, 0, 0], 0), None);
        assert_eq!(set.get_element_index(&[13, 0, 0, 0], 1), None);
        assert_eq!(set.get_element_index(&[9, 0, 0, 0], 0), None);
        assert_eq!(set.get_element_index(&[9, 0, 0, 0], 1), None);
        let addr_type = |_| 0;
        for (key, expected) in [
            ([10, 255, 255, 255], true),
            ([11, 0, 0, 0], true),
            ([11, 255, 255, 255], true),
            ([12, 0, 0, 0], false),
        ] {
            let rule = NftRule {
                handle: 1,
                expressions: alloc::vec![
                    NftExpression::ImmediateData {
                        dreg: 0,
                        data: key.to_vec(),
                    },
                    NftExpression::Lookup {
                        set_handle: set.handle,
                        sreg: 0,
                        key_len: 4,
                        dreg: None,
                        verdict_map: false,
                        invert: false,
                    },
                    NftExpression::Immediate(NftRuleVerdict::Drop),
                ],
            };
            assert_eq!(
                matches!(
                    rule.evaluate(&NftPacket::new(&[0x45; 20], &addr_type), &[set.clone()]),
                    RuleResult::Verdict(NftRuleVerdict::Drop)
                ),
                expected
            );
        }
        let before = Arc::as_ptr(&transaction.candidate.tables[0].sets[0]);
        let overlap = [
            NftSetElementInput {
                key: &[10, 128, 0, 0],
                value: None,
                verdict: None,
                flags: 0,
                key_end: None,
            },
            NftSetElementInput {
                key: &[11, 128, 0, 0],
                value: None,
                verdict: None,
                flags: 1,
                key_end: None,
            },
        ];
        assert!(matches!(
            transaction.update_set_elements(2, b"filter", b"subnet", &overlap, true),
            Err(SystemError::EINVAL)
        ));
        assert_eq!(
            before,
            Arc::as_ptr(&transaction.candidate.tables[0].sets[0])
        );
        assert_eq!(transaction.candidate.tables[0].sets[0].elements.len(), 5);
        transaction
            .update_set_elements(
                2,
                b"filter",
                b"subnet",
                &[NftSetElementInput {
                    key: &[10, 0, 0, 0],
                    value: None,
                    verdict: None,
                    flags: 0,
                    key_end: Some(&[11, 0, 0, 0]),
                }],
                false,
            )
            .unwrap();
        let set = &transaction.candidate.tables[0].sets[0];
        assert_eq!(set.elements.len(), 3);
        assert_eq!(set.element_index(&[11, 0, 0, 0], 0), Some(1));
        assert_eq!(set.element_index(&[11, 0, 0, 0], 1), None);
    }

    #[test]
    fn set_size_limits_distinct_elements_without_partial_mutation() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"small",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: Some(1),
                userdata: &[],
                flags: 0,
                id: None,
                exclusive: true,
            })
            .unwrap();
        let first = NftSetElementInput {
            key: &[127, 0, 0, 1],
            value: None,
            verdict: None,
            flags: 0,
            key_end: None,
        };
        let second = NftSetElementInput {
            key: &[127, 0, 0, 2],
            value: None,
            verdict: None,
            flags: 0,
            key_end: None,
        };
        transaction
            .update_set_elements(2, b"filter", b"small", &[first], true)
            .unwrap();
        assert!(matches!(
            transaction.update_set_elements(2, b"filter", b"small", &[second], true),
            Err(SystemError::ENFILE)
        ));
        assert_eq!(transaction.candidate.tables[0].sets[0].elements.len(), 1);
    }

    #[test]
    fn anonymous_set_ids_bind_distinct_instances_and_retire_with_rules() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(2, b"filter", b"input", None, false, false)
            .unwrap();
        let mut handles = [0u64; 2];
        for (index, id) in [1u32, 2].into_iter().enumerate() {
            let (_, set) = transaction
                .new_set(NewSetSpec {
                    family: 2,
                    table_name: b"filter",
                    name: b"__set%d",
                    key_type: 7,
                    key_len: 4,
                    data_type: None,
                    data_len: None,
                    size: Some(2),
                    userdata: &[],
                    flags: 3,
                    id: Some(id),
                    exclusive: false,
                })
                .unwrap()
                .unwrap();
            assert_ne!(set.name, b"__set%d");
            let name = transaction
                .resolve_set_name(2, b"filter", Some(b"__set%d"), Some(id))
                .unwrap();
            let key = [127, 0, 0, index as u8 + 1];
            transaction
                .update_set_elements(
                    2,
                    b"filter",
                    &name,
                    &[NftSetElementInput {
                        key: &key,
                        value: None,
                        verdict: None,
                        flags: 0,
                        key_end: None,
                    }],
                    true,
                )
                .unwrap();
            let (_, _, rule) = transaction
                .new_ip_rule(
                    2,
                    b"filter",
                    b"input",
                    &[
                        NftExpressionInput::ImmediateData {
                            dreg: 8,
                            data: &key,
                        },
                        NftExpressionInput::Lookup {
                            set: Some(b"__set%d"),
                            set_id: Some(id),
                            sreg: 8,
                            dreg: None,
                            invert: false,
                        },
                    ],
                    true,
                    None,
                )
                .unwrap();
            handles[index] = rule.handle;
            assert!(matches!(
                transaction.update_set_elements(
                    2,
                    b"filter",
                    &name,
                    &[NftSetElementInput {
                        key: &[127, 0, 0, 9],
                        value: None,
                        verdict: None,
                        flags: 0,
                        key_end: None,
                    }],
                    true,
                ),
                Err(SystemError::EBUSY)
            ));
        }
        assert_eq!(transaction.candidate.tables[0].sets.len(), 2);
        assert_ne!(
            transaction.candidate.tables[0].sets[0].name,
            transaction.candidate.tables[0].sets[1].name
        );
        transaction
            .del_rules(2, b"filter", Some(b"input"), Some(handles[0]))
            .unwrap();
        assert_eq!(transaction.candidate.tables[0].sets.len(), 1);
        transaction
            .del_rules(2, b"filter", Some(b"input"), Some(handles[1]))
            .unwrap();
        assert!(transaction.candidate.tables[0].sets.is_empty());
    }

    #[test]
    fn exact_set_single_updates_reuse_candidate_and_error_keeps_batch_private() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"members",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: Some(2),
                userdata: &[],
                flags: 0,
                id: None,
                exclusive: true,
            })
            .unwrap();
        let initial = Arc::as_ptr(&transaction.candidate.tables[0].sets[0]);
        for key in [[127, 0, 0, 1], [127, 0, 0, 2]] {
            transaction
                .update_set_elements(
                    2,
                    b"filter",
                    b"members",
                    &[NftSetElementInput {
                        key: &key,
                        value: None,
                        verdict: None,
                        flags: 0,
                        key_end: None,
                    }],
                    true,
                )
                .unwrap();
        }
        assert_eq!(
            initial,
            Arc::as_ptr(&transaction.candidate.tables[0].sets[0])
        );
        let extra = NftSetElementInput {
            key: &[127, 0, 0, 3],
            value: None,
            verdict: None,
            flags: 0,
            key_end: None,
        };
        assert!(matches!(
            transaction.update_set_elements(2, b"filter", b"members", &[extra], true),
            Err(SystemError::ENFILE)
        ));
        assert_eq!(transaction.candidate.tables[0].sets[0].elements.len(), 2);
        drop(transaction);
        assert!(state.snapshot().tables.is_empty());
    }

    #[test]
    fn multi_element_messages_reuse_private_set_and_roll_back_failed_message() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"members",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: Some(8),
                userdata: &[],
                flags: 0,
                id: None,
                exclusive: true,
            })
            .unwrap();
        let initial = Arc::as_ptr(&transaction.candidate.tables[0].sets[0]);
        for keys in [
            [[127, 0, 0, 1], [127, 0, 0, 2]],
            [[127, 0, 0, 3], [127, 0, 0, 4]],
        ] {
            let inputs = keys.each_ref().map(|key| NftSetElementInput {
                key,
                value: None,
                verdict: None,
                flags: 0,
                key_end: None,
            });
            transaction
                .update_set_elements(2, b"filter", b"members", &inputs, true)
                .unwrap();
            assert_eq!(
                initial,
                Arc::as_ptr(&transaction.candidate.tables[0].sets[0])
            );
        }
        let failed = [[127, 0, 0, 5], [127, 0, 0, 2]];
        let inputs = failed.each_ref().map(|key| NftSetElementInput {
            key,
            value: None,
            verdict: None,
            flags: 0,
            key_end: None,
        });
        assert!(matches!(
            transaction.update_set_elements(2, b"filter", b"members", &inputs, true),
            Err(SystemError::EEXIST)
        ));
        let set = &transaction.candidate.tables[0].sets[0];
        assert_eq!(initial, Arc::as_ptr(set));
        assert_eq!(set.elements.len(), 4);
        assert!(set.elements.iter().all(|element| element.key[3] != 5));
        drop(transaction);
        assert!(state.snapshot().tables.is_empty());
    }

    #[test]
    fn interval_messages_reuse_private_set_and_restore_failed_boundaries() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_set(NewSetSpec {
                family: 2,
                table_name: b"filter",
                name: b"ranges",
                key_type: 7,
                key_len: 4,
                data_type: None,
                data_len: None,
                size: None,
                userdata: &[],
                flags: 4,
                id: None,
                exclusive: true,
            })
            .unwrap();
        let initial = Arc::as_ptr(&transaction.candidate.tables[0].sets[0]);
        for (start, end) in [(10u8, 20u8), (30, 40)] {
            let keys = [[127, 0, 0, start], [127, 0, 0, end]];
            let inputs = [
                NftSetElementInput {
                    key: &keys[0],
                    value: None,
                    verdict: None,
                    flags: 0,
                    key_end: None,
                },
                NftSetElementInput {
                    key: &keys[1],
                    value: None,
                    verdict: None,
                    flags: 1,
                    key_end: None,
                },
            ];
            transaction
                .update_set_elements(2, b"filter", b"ranges", &inputs, true)
                .unwrap();
            assert_eq!(
                initial,
                Arc::as_ptr(&transaction.candidate.tables[0].sets[0])
            );
        }
        let bad = NftSetElementInput {
            key: &[127, 0, 0, 50],
            value: None,
            verdict: None,
            flags: 0,
            key_end: None,
        };
        assert!(matches!(
            transaction.update_set_elements(2, b"filter", b"ranges", &[bad], true),
            Err(SystemError::EINVAL)
        ));
        let set = &transaction.candidate.tables[0].sets[0];
        assert_eq!(initial, Arc::as_ptr(set));
        assert_eq!(set.elements.len(), 4);
        assert_eq!(set.elements[3].key, [127, 0, 0, 40]);
    }
}

#[cfg(test)]
mod register_expression_tests {
    use super::*;

    #[test]
    fn meta_mark_write_is_packet_scoped_and_missing_sidecar_drops() {
        let rule = NftRule {
            handle: 1,
            expressions: alloc::vec![
                NftExpression::ImmediateData {
                    dreg: 0,
                    data: 0x35u32.to_ne_bytes().to_vec(),
                },
                NftExpression::MetaSetMark { sreg: 0 },
                NftExpression::Meta {
                    key: NftMetaKey::Mark,
                    dreg: 0,
                },
                NftExpression::Cmp {
                    sreg: 0,
                    op: NftCmpOp::Eq,
                    data: 0x35u32.to_ne_bytes().to_vec(),
                },
                NftExpression::Immediate(NftRuleVerdict::Accept),
            ],
        };
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type), &[]),
            RuleResult::Verdict(NftRuleVerdict::Drop)
        ));
        let mark = Cell::new(0);
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type).with_mark(&mark), &[]),
            RuleResult::Verdict(NftRuleVerdict::Accept)
        ));
        assert_eq!(mark.get(), 0x35);
    }

    #[test]
    fn meta_protocol_and_nfproto_follow_packet_family() {
        let rule = NftRule {
            handle: 1,
            expressions: alloc::vec![
                NftExpression::Meta {
                    key: NftMetaKey::Protocol,
                    dreg: 0,
                },
                NftExpression::Cmp {
                    sreg: 0,
                    op: NftCmpOp::Eq,
                    data: alloc::vec![0x08, 0x00],
                },
                NftExpression::Meta {
                    key: NftMetaKey::Nfproto,
                    dreg: 0,
                },
                NftExpression::Cmp {
                    sreg: 0,
                    op: NftCmpOp::Eq,
                    data: alloc::vec![2],
                },
                NftExpression::Immediate(NftRuleVerdict::Accept),
            ],
        };
        let ipv4 = [0x45u8; 20];
        let ipv6 = [0x60u8; 40];
        let addr_type = |_| 0;
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&ipv4, &addr_type), &[]),
            RuleResult::Verdict(NftRuleVerdict::Accept)
        ));
        assert!(matches!(
            rule.evaluate(&NftPacket::new_ipv6(&ipv6), &[]),
            RuleResult::Break
        ));
    }

    #[test]
    fn two_byte_meta_protocol_initializes_its_register_slot() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"filter", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(2, b"filter", b"input", None, false, false)
            .unwrap();
        transaction
            .new_ip_rule(
                2,
                b"filter",
                b"input",
                &[
                    NftExpressionInput::Meta { key: 1, dreg: 8 },
                    NftExpressionInput::Cmp {
                        sreg: 8,
                        op: 0,
                        data: &[0x08, 0x00],
                    },
                    NftExpressionInput::Immediate(NftRuleInput::Drop),
                ],
                true,
                None,
            )
            .unwrap();
    }

    #[test]
    fn byteorder_converts_meta_style_host_value_before_comparison() {
        let rule = NftRule {
            handle: 1,
            expressions: alloc::vec![
                NftExpression::ImmediateData {
                    dreg: 0,
                    data: 84u32.to_ne_bytes().to_vec(),
                },
                NftExpression::Byteorder {
                    sreg: 0,
                    dreg: 0,
                    op: NftByteorderOp::HostToNetwork,
                    len: 4,
                    size: 4,
                },
                NftExpression::Cmp {
                    sreg: 0,
                    op: NftCmpOp::Eq,
                    data: 84u32.to_be_bytes().to_vec(),
                },
                NftExpression::Immediate(NftRuleVerdict::Accept),
            ],
        };
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type), &[]),
            RuleResult::Verdict(NftRuleVerdict::Accept)
        ));
    }

    #[test]
    fn negated_range_excludes_both_endpoints() {
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        for (key, expected_match) in [
            ([127, 0, 0, 1], true),
            ([127, 0, 0, 2], false),
            ([127, 0, 0, 3], false),
            ([127, 0, 0, 4], true),
        ] {
            let rule = NftRule {
                handle: 1,
                expressions: alloc::vec![
                    NftExpression::ImmediateData {
                        dreg: 0,
                        data: key.to_vec(),
                    },
                    NftExpression::Range {
                        sreg: 0,
                        op: NftRangeOp::Neq,
                        from: alloc::vec![127, 0, 0, 2],
                        to: alloc::vec![127, 0, 0, 3],
                    },
                    NftExpression::Immediate(NftRuleVerdict::Accept),
                ],
            };
            assert_eq!(
                matches!(
                    rule.evaluate(&NftPacket::new(&packet, &addr_type), &[]),
                    RuleResult::Verdict(NftRuleVerdict::Accept)
                ),
                expected_match
            );
        }
    }
}

#[derive(Debug)]
pub(crate) struct NftTable {
    pub(crate) family: u8,
    pub(crate) name: Vec<u8>,
    pub(crate) handle: u64,
    pub(crate) flags: u32,
    pub(crate) userdata: Vec<u8>,
    pub(crate) use_count: u32,
    pub(crate) chains: Vec<Arc<NftChain>>,
    pub(crate) sets: Vec<Arc<NftSet>>,
    next_handle: u64,
}

/// Named set/map with sorted wire keys. Interval sets store Linux boundary
/// elements (START and `NFT_SET_ELEM_INTERVAL_END`), not key-end records.
#[derive(Debug)]
pub(crate) struct NftSet {
    pub(crate) name: Vec<u8>,
    pub(crate) handle: u64,
    pub(crate) key_type: u32,
    pub(crate) key_len: usize,
    pub(crate) flags: u32,
    pub(crate) data_type: Option<u32>,
    pub(crate) data_len: Option<usize>,
    pub(crate) size: Option<usize>,
    pub(crate) userdata: Vec<u8>,
    pub(crate) elements: Vec<NftSetElement>,
}

#[derive(Debug)]
pub(crate) struct NftSetElement {
    pub(crate) key: Vec<u8>,
    pub(crate) value: Option<Vec<u8>>,
    pub(crate) verdict: Option<NftRuleVerdict>,
    pub(crate) flags: u32,
}

pub(crate) struct NftSetElementInput<'a> {
    pub(crate) key: &'a [u8],
    pub(crate) value: Option<&'a [u8]>,
    pub(crate) verdict: Option<NftRuleInput<'a>>,
    pub(crate) flags: u32,
    pub(crate) key_end: Option<&'a [u8]>,
}

impl NftSet {
    /// Linux interval trees distinguish an end and a start at the same key.
    /// End sorts first, so the start owns the shared address for lookups.
    fn element_position(&self, key: &[u8], flags: u32) -> Result<usize, usize> {
        self.elements.binary_search_by(|candidate| {
            candidate.key.as_slice().cmp(key).then_with(|| {
                if self.flags & 4 != 0 {
                    (candidate.flags & 1 == 0).cmp(&(flags & 1 == 0))
                } else {
                    core::cmp::Ordering::Equal
                }
            })
        })
    }

    pub(crate) fn element_index(&self, key: &[u8], flags: u32) -> Option<usize> {
        self.element_position(key, flags).ok()
    }

    /// GETSETELEM uses interval containment, unlike exact deletion and events.
    /// Linux's nft_rbtree_get returns the preceding START or following END.
    pub(crate) fn get_element_index(&self, key: &[u8], flags: u32) -> Option<usize> {
        if key.len() != self.key_len || flags & !1 != 0 {
            return None;
        }
        if let Some(index) = self.element_index(key, flags) {
            return Some(index);
        }
        if self.flags & 4 == 0 {
            return None;
        }
        let upper = self
            .elements
            .partition_point(|element| element.key.as_slice() <= key);
        let index = if flags & 1 != 0 {
            upper
        } else {
            upper.checked_sub(1)?
        };
        self.elements
            .get(index)
            .filter(|element| element.flags & 1 == flags & 1)
            .map(|_| index)
    }

    fn clone_for_write(&self) -> Result<Arc<Self>, SystemError> {
        let mut elements = Vec::new();
        elements
            .try_reserve_exact(self.elements.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for element in &self.elements {
            elements.push(NftSetElement {
                key: copy_bytes(&element.key)?,
                value: element
                    .value
                    .as_ref()
                    .map(|value| copy_bytes(value))
                    .transpose()?,
                verdict: element.verdict,
                flags: element.flags,
            });
        }
        Arc::try_new(Self {
            name: copy_bytes(&self.name)?,
            handle: self.handle,
            key_type: self.key_type,
            key_len: self.key_len,
            flags: self.flags,
            data_type: self.data_type,
            data_len: self.data_len,
            size: self.size,
            userdata: copy_bytes(&self.userdata)?,
            elements,
        })
        .map_err(|_| SystemError::ENOMEM)
    }
}

/// Regular chains are inert until an executable jump expression references
/// them. A base chain records the exact hook at which it executes.
#[derive(Debug)]
pub(crate) struct NftChain {
    pub(crate) name: Vec<u8>,
    pub(crate) handle: u64,
    pub(crate) base: Option<NftBaseChain>,
    rules: Vec<Arc<NftRule>>,
}

pub(crate) type CreatedChain = (Arc<NftTable>, Arc<NftChain>);
pub(crate) type CreatedRule = (Arc<NftTable>, Arc<NftChain>, Arc<NftRule>);

/// A regular chain has no packet hook or fallthrough policy. Keeping these
/// fields together prevents it from being registered as a base chain when
/// regular chains become available to the nftables control plane.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NftBaseChain {
    hook_order: u64,
    pub(crate) hook: NftIpv4Hook,
    pub(crate) priority: i32,
    pub(crate) policy: NftVerdict,
    pub(crate) chain_type: NftChainType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftChainType {
    Filter,
    Nat,
}

/// IPv4 Netfilter hook numbers are a Linux UAPI, not an internal ordering
/// chosen by the ruleset compiler. Only wired hooks are accepted by netlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum NftIpv4Hook {
    PreRouting = 0,
    LocalIn = 1,
    Forward = 2,
    LocalOut = 3,
    PostRouting = 4,
}

impl NftIpv4Hook {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug)]
pub(crate) struct NftRule {
    pub(crate) handle: u64,
    expressions: Vec<NftExpression>,
}

impl NftRule {
    fn requires_conntrack(&self) -> bool {
        self.expressions.iter().any(|expression| {
            matches!(
                expression,
                NftExpression::CtState { .. }
                    | NftExpression::XtConntrack(_)
                    | NftExpression::Nat { .. }
                    | NftExpression::Masquerade { .. }
                    | NftExpression::Redirect { .. }
                    | NftExpression::XtNatTarget { .. }
            )
        })
    }

    pub(crate) fn verdict(&self) -> NftRuleVerdict {
        self.expressions
            .iter()
            .find_map(|expression| match expression {
                NftExpression::Immediate(verdict) => Some(*verdict),
                _ => None,
            })
            .unwrap_or(NftRuleVerdict::Continue)
    }

    fn evaluate(&self, context: &NftPacket<'_>, sets: &[Arc<NftSet>]) -> RuleResult {
        let packet = context.bytes;
        // nft data registers: four legacy 16-byte registers or sixteen
        // 32-bit registers, both addressing the same 64 bytes.
        let mut registers = [0u8; 64];
        let mut transport_offset = None;
        for expression in &self.expressions {
            match expression {
                NftExpression::Payload {
                    base,
                    offset,
                    len,
                    dreg,
                } => {
                    let start = match base {
                        NftPayloadBase::Network => *offset,
                        NftPayloadBase::Transport => {
                            let Some(header_len) = *transport_offset
                                .get_or_insert_with(|| ip_transport_offset(packet))
                            else {
                                return RuleResult::Break;
                            };
                            header_len + offset
                        }
                    };
                    let Some(source) = packet.get(start..start + len) else {
                        return RuleResult::Break;
                    };
                    registers[*dreg..dreg + len].copy_from_slice(source);
                    let aligned_end = (len + 3) & !3;
                    registers[dreg + len..dreg + aligned_end].fill(0);
                }
                NftExpression::Cmp { sreg, op, data } => {
                    let comparison = registers[*sreg..sreg + data.len()].cmp(data);
                    let matches = match op {
                        NftCmpOp::Eq => comparison.is_eq(),
                        NftCmpOp::Neq => !comparison.is_eq(),
                        NftCmpOp::Lt => comparison.is_lt(),
                        NftCmpOp::Lte => !comparison.is_gt(),
                        NftCmpOp::Gt => comparison.is_gt(),
                        NftCmpOp::Gte => !comparison.is_lt(),
                    };
                    if !matches {
                        return RuleResult::Break;
                    }
                }
                NftExpression::Byteorder {
                    sreg,
                    dreg,
                    len,
                    size,
                    ..
                } => {
                    for index in 0..len / size {
                        // Linux 6.6 indexes the 64-bit source through u32
                        // slots, while its destination uses u64 slots.
                        let source = sreg + index * if *size == 8 { 4 } else { *size };
                        let destination = dreg + index * size;
                        let mut value = [0u8; 8];
                        value[..*size].copy_from_slice(&registers[source..source + size]);
                        if cfg!(target_endian = "little") {
                            value[..*size].reverse();
                        }
                        registers[destination..destination + size].copy_from_slice(&value[..*size]);
                    }
                }
                NftExpression::Range { sreg, op, from, to } => {
                    let value = &registers[*sreg..sreg + from.len()];
                    let inside = value >= from.as_slice() && value <= to.as_slice();
                    if inside != matches!(op, NftRangeOp::Eq) {
                        return RuleResult::Break;
                    }
                }
                NftExpression::Bitwise {
                    sreg,
                    dreg,
                    len,
                    operation,
                } => {
                    let words = len.div_ceil(4);
                    // Linux evaluates whole u32 slots directly against the
                    // register file. Its traversal order is observable when
                    // source and destination ranges partially overlap.
                    match operation {
                        NftBitwiseOperation::Bool { mask, xor } => {
                            for index in 0..words {
                                let start = index * 4;
                                let src_offset = sreg + start;
                                let input = u32::from_ne_bytes(
                                    registers[src_offset..src_offset + 4].try_into().unwrap(),
                                );
                                let mut mask_word = [0u8; 4];
                                let mut xor_word = [0u8; 4];
                                let available = len.saturating_sub(start).min(4);
                                mask_word[..available]
                                    .copy_from_slice(&mask[start..start + available]);
                                xor_word[..available]
                                    .copy_from_slice(&xor[start..start + available]);
                                let output = (input & u32::from_ne_bytes(mask_word))
                                    ^ u32::from_ne_bytes(xor_word);
                                let offset = dreg + index * 4;
                                registers[offset..offset + 4]
                                    .copy_from_slice(&output.to_ne_bytes());
                            }
                        }
                        NftBitwiseOperation::Lshift(shift) => {
                            let mut carry = 0;
                            for index in (0..words).rev() {
                                let src_offset = sreg + index * 4;
                                let input = u32::from_ne_bytes(
                                    registers[src_offset..src_offset + 4].try_into().unwrap(),
                                );
                                let output = (input << shift) | carry;
                                carry = if *shift == 0 {
                                    0
                                } else {
                                    input >> (32 - shift)
                                };
                                let offset = dreg + index * 4;
                                registers[offset..offset + 4]
                                    .copy_from_slice(&output.to_ne_bytes());
                            }
                        }
                        NftBitwiseOperation::Rshift(shift) => {
                            let mut carry = 0;
                            for index in 0..words {
                                let src_offset = sreg + index * 4;
                                let input = u32::from_ne_bytes(
                                    registers[src_offset..src_offset + 4].try_into().unwrap(),
                                );
                                let output = carry | (input >> shift);
                                carry = if *shift == 0 {
                                    0
                                } else {
                                    input << (32 - shift)
                                };
                                let offset = dreg + index * 4;
                                registers[offset..offset + 4]
                                    .copy_from_slice(&output.to_ne_bytes());
                            }
                        }
                    }
                }
                NftExpression::Counter(counter) => {
                    counter.count(packet.len() as u64);
                }
                NftExpression::CtState { dreg } => {
                    registers[*dreg..*dreg + 4]
                        .copy_from_slice(&context.ct_state_bits().to_ne_bytes());
                }
                NftExpression::Meta { key, dreg } => match key {
                    NftMetaKey::Len => registers[*dreg..*dreg + 4]
                        .copy_from_slice(&(packet.len() as u32).to_ne_bytes()),
                    NftMetaKey::Protocol => {
                        let protocol: u16 = match packet.first().map(|byte| byte >> 4) {
                            Some(4) => 0x0800,
                            Some(6) => 0x86dd,
                            _ => return RuleResult::Break,
                        };
                        registers[*dreg..*dreg + 4].fill(0);
                        registers[*dreg..*dreg + 2].copy_from_slice(&protocol.to_be_bytes());
                    }
                    NftMetaKey::Mark => {
                        let Some(mark) = context.mark else {
                            return RuleResult::Verdict(NftRuleVerdict::Drop);
                        };
                        registers[*dreg..*dreg + 4].copy_from_slice(&mark.get().to_ne_bytes());
                    }
                    NftMetaKey::Nfproto => {
                        let family = match packet.first().map(|byte| byte >> 4) {
                            Some(4) => 2u8,
                            Some(6) => 10u8,
                            _ => return RuleResult::Break,
                        };
                        registers[*dreg..*dreg + 4].fill(0);
                        registers[*dreg] = family;
                    }
                    NftMetaKey::Iifname => {
                        registers[*dreg..*dreg + 16].copy_from_slice(&context.iifname)
                    }
                    NftMetaKey::Oifname => {
                        registers[*dreg..*dreg + 16].copy_from_slice(&context.oifname)
                    }
                    NftMetaKey::L4proto => {
                        let Some(proto) = ip_protocol(packet) else {
                            return RuleResult::Break;
                        };
                        registers[*dreg..*dreg + 4].fill(0);
                        registers[*dreg] = proto;
                    }
                },
                NftExpression::MetaSetMark { sreg } => {
                    let Some(mark) = context.mark else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    mark.set(u32::from_ne_bytes(
                        registers[*sreg..*sreg + 4].try_into().unwrap(),
                    ));
                }
                NftExpression::FibDaddrType { dreg } => {
                    let Some(addr_type) = context.ipv4_addr_type else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Ok(ipv4) = smoltcp::wire::Ipv4Packet::new_checked(packet) else {
                        return RuleResult::Break;
                    };
                    registers[*dreg..*dreg + 4]
                        .copy_from_slice(&(addr_type(ipv4.dst_addr()) as u32).to_ne_bytes());
                }
                NftExpression::XtTcp(tcp) => match tcp.evaluate(packet) {
                    RuleResult::Continue => {}
                    other => return other,
                },
                NftExpression::XtAddrtype(matcher) => match matcher.evaluate(context) {
                    Some(true) => {}
                    Some(false) => return RuleResult::Break,
                    None => return RuleResult::Verdict(NftRuleVerdict::Drop),
                },
                NftExpression::XtConntrack(matcher) => {
                    if !matcher.evaluate(context) {
                        return RuleResult::Break;
                    }
                }
                NftExpression::Nat {
                    side,
                    family,
                    addr_min,
                    addr_max,
                    port_min,
                    port_max,
                } => {
                    if packet.first().map(|byte| byte >> 4)
                        != Some(if *family == 2 { 4 } else { 6 })
                    {
                        // nft_nat_inet_eval leaves the other protocol alone.
                        continue;
                    }
                    let address = if let Some(min) = addr_min {
                        let len = if *family == 2 { 4 } else { 16 };
                        if addr_max.is_some_and(|max| {
                            registers[*min..*min + len] != registers[max..max + len]
                        }) {
                            return RuleResult::Verdict(NftRuleVerdict::Drop);
                        }
                        Some(if *family == 2 {
                            CtAddress::V4(registers[*min..*min + 4].try_into().unwrap())
                        } else {
                            CtAddress::V6(registers[*min..*min + 16].try_into().unwrap())
                        })
                    } else {
                        None
                    };
                    let Some(ports) = nat_ports(&registers, *port_min, *port_max) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Ok(request) = CtNatRequest::new(address, ports) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    return RuleResult::Nat(match side {
                        NatManipSide::Destination => NftNatAction::Dnat(request),
                        NatManipSide::Source => NftNatAction::Snat(request),
                    });
                }
                NftExpression::Masquerade { port_min, port_max } => {
                    let Some(ports) = nat_ports(&registers, *port_min, *port_max) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    return RuleResult::Nat(NftNatAction::Masquerade { ports });
                }
                NftExpression::Redirect {
                    port_min, port_max, ..
                } => {
                    let Some(resolve) = context.redirect_address else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Some(address) = resolve(packet) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    if !matches!(
                        (packet.first().map(|byte| byte >> 4), address),
                        (Some(4), CtAddress::V4(_)) | (Some(6), CtAddress::V6(_))
                    ) {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    }
                    let Some(ports) = nat_ports(&registers, *port_min, *port_max) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let Ok(request) = CtNatRequest::new(Some(address), ports) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    return RuleResult::Nat(NftNatAction::Dnat(request));
                }
                NftExpression::XtNatTarget { action, .. } => return RuleResult::Nat(*action),
                NftExpression::ImmediateData { dreg, data } => {
                    registers[*dreg..*dreg + data.len()].copy_from_slice(data);
                    let aligned_end = (data.len() + 3) & !3;
                    registers[dreg + data.len()..dreg + aligned_end].fill(0);
                }
                NftExpression::Lookup {
                    set_handle,
                    sreg,
                    key_len,
                    dreg,
                    verdict_map,
                    invert,
                } => {
                    // Table sets are appended with monotonically increasing
                    // handles; deletion preserves order and COW replaces in
                    // place. Keep packet lookup logarithmic in set count.
                    let Ok(index) = sets.binary_search_by_key(set_handle, |set| set.handle) else {
                        return RuleResult::Verdict(NftRuleVerdict::Drop);
                    };
                    let set = &sets[index];
                    let found = if set.flags & 4 != 0 {
                        let key = &registers[*sreg..*sreg + *key_len];
                        let floor = set
                            .elements
                            .partition_point(|element| element.key.as_slice() <= key)
                            .checked_sub(1);
                        floor.filter(|index| set.elements[*index].flags & 1 == 0)
                    } else {
                        set.element_index(&registers[*sreg..*sreg + *key_len], 0)
                    };
                    match (found, dreg) {
                        (Some(index), Some(0)) if *verdict_map => {
                            let Some(verdict) = set.elements[index].verdict else {
                                return RuleResult::Verdict(NftRuleVerdict::Drop);
                            };
                            return RuleResult::Verdict(verdict);
                        }
                        (Some(index), Some(destination)) => {
                            let Some(value) = set.elements[index].value.as_ref() else {
                                return RuleResult::Verdict(NftRuleVerdict::Drop);
                            };
                            registers[*destination..*destination + value.len()]
                                .copy_from_slice(value);
                            let aligned_end = (value.len() + 3) & !3;
                            registers[destination + value.len()..destination + aligned_end].fill(0);
                        }
                        (found, None) if found.is_some() != *invert => {}
                        _ => return RuleResult::Break,
                    }
                }
                NftExpression::Immediate(verdict) => return RuleResult::Verdict(*verdict),
            }
        }
        RuleResult::Continue
    }

    pub(crate) fn expressions(&self) -> &[NftExpression] {
        &self.expressions
    }
}

/// `None` means an invalid register range; `Some(None)` means no port mapping.
fn nat_ports(
    registers: &[u8; 64],
    min: Option<usize>,
    max: Option<usize>,
) -> Option<Option<CtNatPortRange>> {
    let Some(min) = min else { return Some(None) };
    let first = u16::from_be_bytes(registers[min..min + 2].try_into().unwrap());
    let last = max.map_or(first, |index| {
        u16::from_be_bytes(registers[index..index + 2].try_into().unwrap())
    });
    CtNatPortRange::new(first, last).ok().map(Some)
}

/// Accept only the xt target layouts and range semantics represented by
/// CtNatRequest. In particular random, persistent and address ranges are
/// never silently converted to a single-address mapping.
fn parse_xt_nat_target(
    family: u8,
    name: &[u8],
    revision: u32,
    info: &[u8],
) -> Result<NftNatAction, SystemError> {
    if family == 2 && (name == b"DNAT" || name == b"SNAT") && revision == 0 {
        if !(20..=24).contains(&info.len()) || info[20..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        if u32::from_ne_bytes(info[..4].try_into().unwrap()) != 1 {
            return Err(SystemError::EINVAL);
        }
        let flags = u32::from_ne_bytes(info[4..8].try_into().unwrap());
        if flags & !3 != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let address = if flags & 1 != 0 {
            if info[8..12] != info[12..16] {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            Some(CtAddress::V4(info[8..12].try_into().unwrap()))
        } else {
            if info[8..16].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        let ports = if flags & 2 != 0 {
            Some(
                CtNatPortRange::new(
                    u16::from_be_bytes(info[16..18].try_into().unwrap()),
                    u16::from_be_bytes(info[18..20].try_into().unwrap()),
                )
                .map_err(|_| SystemError::EINVAL)?,
            )
        } else {
            if info[16..20].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        let request = CtNatRequest::new(address, ports).map_err(|_| SystemError::EINVAL)?;
        return Ok(if name == b"DNAT" {
            NftNatAction::Dnat(request)
        } else {
            NftNatAction::Snat(request)
        });
    }
    if family == 2 && name == b"MASQUERADE" && revision == 0 {
        if !(20..=24).contains(&info.len()) || info[20..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        if u32::from_ne_bytes(info[..4].try_into().unwrap()) != 1 {
            return Err(SystemError::EINVAL);
        }
        let flags = u32::from_ne_bytes(info[4..8].try_into().unwrap());
        if flags & !2 != 0 || info[8..16].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let ports = if flags & 2 != 0 {
            Some(
                CtNatPortRange::new(
                    u16::from_be_bytes(info[16..18].try_into().unwrap()),
                    u16::from_be_bytes(info[18..20].try_into().unwrap()),
                )
                .map_err(|_| SystemError::EINVAL)?,
            )
        } else {
            if info[16..20].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        return Ok(NftNatAction::Masquerade { ports });
    }
    if family == 10 && name == b"DNAT" && revision == 2 {
        if !(44..=48).contains(&info.len()) || info[44..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        let flags = u32::from_ne_bytes(info[..4].try_into().unwrap());
        if flags & !3 != 0
            || flags & 1 == 0
            || info[4..20] != info[20..36]
            || info[40..44].iter().any(|byte| *byte != 0)
        {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let ports = if flags & 2 != 0 {
            Some(
                CtNatPortRange::new(
                    u16::from_be_bytes(info[36..38].try_into().unwrap()),
                    u16::from_be_bytes(info[38..40].try_into().unwrap()),
                )
                .map_err(|_| SystemError::EINVAL)?,
            )
        } else {
            if info[36..40].iter().any(|byte| *byte != 0) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            None
        };
        let address = CtAddress::V6(info[4..20].try_into().unwrap());
        let request = CtNatRequest::new(Some(address), ports).map_err(|_| SystemError::EINVAL)?;
        return Ok(NftNatAction::Dnat(request));
    }
    Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
}

fn ipv4_transport_offset(packet: &[u8]) -> Option<usize> {
    let first = *packet.first()?;
    let header_len = usize::from(first & 0x0f) * 4;
    if first >> 4 != 4 || header_len < 20 || packet.len() < header_len {
        return None;
    }
    let fragment = packet.get(6..8)?;
    if fragment[0] & 0x1f != 0 || fragment[1] != 0 {
        return None;
    }
    Some(header_len)
}

/// Match Linux 6.6 `ipv6_find_hdr(..., target = -1, IP6_FH_F_AUTH)`:
/// AH is the terminal protocol, while non-first fragments expose their
/// next-header value to meta but cannot expose transport payload bytes.
fn ipv6_transport_info(packet: &[u8]) -> Option<(u8, usize, bool)> {
    if packet.len() < 40 || packet[0] >> 4 != 6 {
        return None;
    }
    let payload_len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    if packet.len() < 40 + payload_len || (payload_len == 0 && packet[6] == 0) {
        // A zero-length fixed header is valid unless it indicates a Jumbo
        // Payload hop-by-hop option, which this receive path cannot parse.
        return None;
    }
    let end = 40 + payload_len;
    let packet = &packet[..end];
    let mut next = packet[6];
    let mut offset = 40;
    loop {
        match next {
            0 | 43 | 60 | 44 | 51 => {
                let header = packet.get(offset..offset + 2)?;
                if next == 51 {
                    // Linux's nft packet-info parser stops at AH.
                    return Some((next, offset, false));
                }
                let new_next = header[0];
                let len = if next == 44 {
                    let fragment = packet.get(offset + 2..offset + 4)?;
                    if u16::from_be_bytes([fragment[0], fragment[1]]) & 0xfff8 != 0 {
                        if matches!(new_next, 0 | 43 | 44 | 51 | 60) {
                            return None;
                        }
                        // ipv6_find_hdr returns here without publishing its
                        // local `start` through the output offset pointer.
                        return Some((new_next, 0, true));
                    }
                    8
                } else {
                    (usize::from(header[1]) + 1) * 8
                };
                offset = offset.checked_add(len)?;
                if offset > end {
                    return None;
                }
                next = new_next;
            }
            _ => return Some((next, offset, false)),
        }
    }
}

fn ip_transport_offset(packet: &[u8]) -> Option<usize> {
    match packet.first()? >> 4 {
        4 => ipv4_transport_offset(packet),
        6 => ipv6_transport_info(packet)
            .and_then(|(_, offset, fragment)| (!fragment).then_some(offset)),
        _ => None,
    }
}

#[cfg(test)]
mod ipv6_packet_info_tests {
    use super::{ip_protocol, ip_transport_offset, ipv6_transport_info};

    fn packet(next: u8, payload: &[u8]) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec![0; 40];
        bytes[0] = 0x60;
        bytes[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        bytes[6] = next;
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn extension_chain_locates_transport_without_exposing_padding() {
        let mut payload = [0u8; 24];
        payload[0] = 43; // Hop-by-hop -> routing.
        payload[8] = 60; // Routing -> destination options.
        payload[16] = 17; // Destination options -> UDP.
        let bytes = packet(0, &payload);
        assert_eq!(ip_protocol(&bytes), Some(17));
        assert_eq!(ip_transport_offset(&bytes), Some(64));
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0; 16]);
        assert_eq!(ip_transport_offset(&padded), Some(64));
        padded[4..6].copy_from_slice(&8u16.to_be_bytes());
        assert_eq!(ip_protocol(&padded), None);
    }

    #[test]
    fn nonfirst_fragment_retains_meta_protocol_but_not_transport_payload() {
        let mut fragment = [0u8; 8];
        fragment[0] = 17;
        fragment[2..4].copy_from_slice(&8u16.to_be_bytes());
        let bytes = packet(44, &fragment);
        assert_eq!(ipv6_transport_info(&bytes), Some((17, 0, true)));
        assert_eq!(ip_protocol(&bytes), Some(17));
        assert_eq!(ip_transport_offset(&bytes), None);
        fragment[0] = 60; // An extension after a nonfirst fragment is unknown.
        assert_eq!(ip_protocol(&packet(44, &fragment)), None);
    }

    #[test]
    fn authentication_header_is_terminal_for_linux_nft_packet_info() {
        let bytes = packet(51, &[17, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(ipv6_transport_info(&bytes), Some((51, 40, false)));
        assert_eq!(ip_transport_offset(&bytes), Some(40));
    }

    #[test]
    fn empty_and_truncated_ipv6_payloads_do_not_read_l2_padding() {
        assert_eq!(ip_protocol(&packet(59, &[])), Some(59));
        assert_eq!(ip_transport_offset(&packet(17, &[])), Some(40));
        assert_eq!(ip_protocol(&packet(0, &[])), None);

        let mut truncated = packet(60, &[17, 1, 0, 0, 0, 0, 0, 0]);
        truncated.extend_from_slice(&[0; 8]); // Link-layer padding is not payload.
        assert_eq!(ip_protocol(&truncated), None);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleResult {
    Break,
    Continue,
    Verdict(NftRuleVerdict),
    Nat(NftNatAction),
}

enum ChainResult {
    Verdict(NftVerdict),
    Nat(NftNatAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftNatAction {
    Dnat(CtNatRequest),
    Snat(CtNatRequest),
    Masquerade { ports: Option<CtNatPortRange> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftNatEvent {
    Begin(NatManipSide),
    Rule(NftNatAction),
    Finish(NatManipSide),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftNatProgress {
    Continue,
    SkipRules,
}

/// Names in the netlink request are resolved against the transaction's
/// candidate table. Published rules refer to stable handles, never to an Arc
/// of a chain that a later copy-on-write update may replace.
#[derive(Clone, Copy)]
pub(crate) enum NftRuleInput<'a> {
    Accept,
    Drop,
    Continue,
    Return,
    Jump(&'a [u8]),
    Goto(&'a [u8]),
}

pub(crate) enum NftExpressionInput<'a> {
    Immediate(NftRuleInput<'a>),
    ImmediateData {
        dreg: u32,
        data: &'a [u8],
    },
    Lookup {
        set: Option<&'a [u8]>,
        set_id: Option<u32>,
        sreg: u32,
        dreg: Option<u32>,
        invert: bool,
    },
    XtTcp(&'a [u8]),
    XtAddrtype(&'a [u8]),
    XtConntrack {
        revision: u32,
        info: &'a [u8],
    },
    Ct {
        key: u32,
        dreg: u32,
        direction: Option<u8>,
    },
    Nat {
        nat_type: u32,
        family: u32,
        addr_min_reg: Option<u32>,
        addr_max_reg: Option<u32>,
        proto_min_reg: Option<u32>,
        proto_max_reg: Option<u32>,
        flags: u32,
    },
    Masq {
        flags: u32,
        proto_min_reg: Option<u32>,
        proto_max_reg: Option<u32>,
    },
    Redirect {
        flags: Option<u32>,
        proto_min_reg: Option<u32>,
        proto_max_reg: Option<u32>,
    },
    XtTarget {
        name: &'a [u8],
        revision: u32,
        info: &'a [u8],
    },
    Meta {
        key: u32,
        dreg: u32,
    },
    MetaSet {
        key: u32,
        sreg: u32,
    },
    Fib {
        dreg: u32,
        result: u32,
        flags: u32,
    },
    Payload {
        dreg: u32,
        base: u32,
        offset: u32,
        len: u32,
    },
    Cmp {
        sreg: u32,
        op: u32,
        data: &'a [u8],
    },
    Byteorder {
        sreg: u32,
        dreg: u32,
        op: u32,
        len: u32,
        size: u32,
    },
    Range {
        sreg: u32,
        op: u32,
        from: &'a [u8],
        to: &'a [u8],
    },
    Bitwise {
        sreg: u32,
        dreg: u32,
        len: u32,
        op: u32,
        mask: Option<&'a [u8]>,
        xor: Option<&'a [u8]>,
        data: Option<&'a [u8]>,
    },
    Counter {
        bytes: u64,
        packets: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftRuleVerdict {
    Accept,
    Drop,
    Continue,
    Return,
    Jump(u64),
    Goto(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NftVerdict {
    Accept,
    Drop,
}

#[derive(Debug)]
pub(crate) enum NftExpression {
    Immediate(NftRuleVerdict),
    ImmediateData {
        dreg: usize,
        data: Vec<u8>,
    },
    Lookup {
        set_handle: u64,
        sreg: usize,
        key_len: usize,
        dreg: Option<usize>,
        verdict_map: bool,
        invert: bool,
    },
    XtTcp(NftXtTcp),
    XtAddrtype(NftXtAddrtype),
    XtConntrack(NftXtConntrack),
    CtState {
        dreg: usize,
    },
    Nat {
        side: NatManipSide,
        family: u8,
        addr_min: Option<usize>,
        addr_max: Option<usize>,
        port_min: Option<usize>,
        port_max: Option<usize>,
    },
    Masquerade {
        port_min: Option<usize>,
        port_max: Option<usize>,
    },
    Redirect {
        flags: u32,
        port_min: Option<usize>,
        port_max: Option<usize>,
    },
    XtNatTarget {
        name: &'static [u8],
        revision: u32,
        info: Vec<u8>,
        action: NftNatAction,
    },
    Meta {
        key: NftMetaKey,
        dreg: usize,
    },
    MetaSetMark {
        sreg: usize,
    },
    FibDaddrType {
        dreg: usize,
    },
    Payload {
        base: NftPayloadBase,
        offset: usize,
        len: usize,
        dreg: usize,
    },
    Cmp {
        sreg: usize,
        op: NftCmpOp,
        data: Vec<u8>,
    },
    Byteorder {
        sreg: usize,
        dreg: usize,
        len: usize,
        size: usize,
        op: NftByteorderOp,
    },
    Range {
        sreg: usize,
        op: NftRangeOp,
        from: Vec<u8>,
        to: Vec<u8>,
    },
    Bitwise {
        sreg: usize,
        dreg: usize,
        len: usize,
        operation: NftBitwiseOperation,
    },
    Counter(NftCounter),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftMetaKey {
    Len,
    Protocol,
    Mark,
    Nfproto,
    Iifname,
    Oifname,
    L4proto,
}

impl NftMetaKey {
    pub(crate) const fn uapi(self) -> u32 {
        match self {
            Self::Len => 0,
            Self::Protocol => 1,
            Self::Mark => 3,
            Self::Nfproto => 15,
            Self::Iifname => 6,
            Self::Oifname => 7,
            Self::L4proto => 16,
        }
    }
}

fn ipv4_protocol(packet: &[u8]) -> Option<u8> {
    let header = *packet.first()?;
    let header_len = usize::from(header & 0x0f) * 4;
    if header >> 4 != 4 || header_len < 20 || packet.len() < header_len {
        return None;
    }
    packet.get(9).copied()
}

fn ip_protocol(packet: &[u8]) -> Option<u8> {
    match packet.first()? >> 4 {
        4 => ipv4_protocol(packet),
        6 => ipv6_transport_info(packet).map(|(protocol, _, _)| protocol),
        _ => None,
    }
}

/// iptables-nft's `addrtype --dst-type LOCAL` uses xt revision 1 on the
/// wire, even though `nft list ruleset` prints a `fib` shorthand. Until the
/// other addrtype masks and interface restrictions have routing-context tests,
/// reject them instead of claiming a rule with different semantics works.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NftXtAddrtype {
    destination_mask: u16,
}

impl NftXtAddrtype {
    fn from_info(info: &[u8]) -> Result<Self, SystemError> {
        if info.len() != 8 {
            return Err(SystemError::EINVAL);
        }
        let source_mask = u16::from_ne_bytes([info[0], info[1]]);
        let destination_mask = u16::from_ne_bytes([info[2], info[3]]);
        let flags = u32::from_ne_bytes(info[4..8].try_into().unwrap());
        if source_mask != 0 || destination_mask != (1 << crate::net::route::RTN_LOCAL) || flags != 0
        {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        Ok(Self { destination_mask })
    }

    pub(crate) fn info(&self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[2..4].copy_from_slice(&self.destination_mask.to_ne_bytes());
        bytes
    }

    /// `None` means the packet path omitted its protected routing view. That
    /// is a hard drop, not a failed match that could bypass a firewall jump.
    fn evaluate(&self, context: &NftPacket<'_>) -> Option<bool> {
        match context.bytes.first().map(|byte| byte >> 4) {
            Some(4) => {
                let Ok(packet) = smoltcp::wire::Ipv4Packet::new_checked(context.bytes) else {
                    return Some(false);
                };
                let addr_type = context.ipv4_addr_type?;
                let kind = addr_type(packet.dst_addr());
                Some(self.destination_mask & (1u16 << kind.min(15)) != 0)
            }
            Some(6) => {
                let Ok(packet) = smoltcp::wire::Ipv6Packet::new_checked(context.bytes) else {
                    return Some(false);
                };
                let is_local = context.ipv6_local_destination?;
                Some(is_local(packet.dst_addr()))
            }
            _ => Some(false),
        }
    }
}

/// The state-only subset of Linux's xt_conntrack revisions 1-3. Reject any
/// tuple, status, expiry or NAT-state predicate until its execution exists.
#[derive(Debug)]
pub(crate) struct NftXtConntrack {
    revision: u32,
    info: Vec<u8>,
    state_mask: u16,
    invert: bool,
}

impl NftXtConntrack {
    fn from_info(revision: u32, info: &[u8]) -> Result<Self, SystemError> {
        let size = match revision {
            1 => 152,
            2 => 156,
            3 => 164,
            _ => return Err(SystemError::ENOENT),
        };
        let aligned = (size + 7) & !7;
        if info.len() < size || info.len() > aligned || info[size..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }
        // xt_conntrack_mtinfo{1,2,3}: four address/mask pairs occupy
        // 0..128, expiry 128..136, protocol 136..138, ports 138..146.
        if info[..146].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let match_flags = u16::from_ne_bytes(info[146..148].try_into().unwrap());
        let invert_flags = u16::from_ne_bytes(info[148..150].try_into().unwrap());
        let state_mask = if revision == 1 {
            if info[151] != 0 {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            u16::from(info[150])
        } else {
            if info[152..156].iter().any(|byte| *byte != 0)
                || (revision == 3 && info[156..164].iter().any(|byte| *byte != 0))
            {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            u16::from_ne_bytes(info[150..152].try_into().unwrap())
        };
        // Linux's UNTRACKED bit is 1 << 9 in xt, versus 1 << 6 in
        // nf_tables. SNAT/DNAT bits require NAT status and remain unsupported.
        if match_flags != 1 || invert_flags & !1 != 0 || state_mask & !0x020f != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        Ok(Self {
            revision,
            info: copy_bytes(info)?,
            state_mask,
            invert: invert_flags != 0,
        })
    }

    pub(crate) fn revision(&self) -> u32 {
        self.revision
    }

    pub(crate) fn info(&self) -> &[u8] {
        &self.info
    }

    fn evaluate(&self, context: &NftPacket<'_>) -> bool {
        let native = context.ct_state_bits();
        let xt_state = (native & 0x0f) | ((native & (1 << 6)) << 3);
        (self.state_mask & xt_state as u16 != 0) ^ self.invert
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NftXtTcp {
    source_ports: [u16; 2],
    dest_ports: [u16; 2],
    option: u8,
    flags_mask: u8,
    flags_match: u8,
    invert: u8,
}

impl NftXtTcp {
    fn from_info(info: &[u8]) -> Result<Self, SystemError> {
        // xt_check_match compares XT_ALIGN(user length) with the aligned
        // 12-byte xt_tcp matchsize. Its four port bounds are host-endian.
        if !(12..=16).contains(&info.len()) {
            return Err(SystemError::EINVAL);
        }
        if info[11] & !0x0f != 0 {
            return Err(SystemError::EINVAL);
        }
        let port = |index| u16::from_ne_bytes([info[index], info[index + 1]]);
        Ok(Self {
            source_ports: [port(0), port(2)],
            dest_ports: [port(4), port(6)],
            option: info[8],
            flags_mask: info[9],
            flags_match: info[10],
            invert: info[11],
        })
    }

    pub(crate) fn info(&self) -> [u8; 16] {
        let mut info = [0u8; 16];
        for (index, port) in self
            .source_ports
            .iter()
            .chain(self.dest_ports.iter())
            .enumerate()
        {
            info[index * 2..index * 2 + 2].copy_from_slice(&port.to_ne_bytes());
        }
        info[8..12].copy_from_slice(&[self.option, self.flags_mask, self.flags_match, self.invert]);
        info
    }

    fn evaluate(&self, packet: &[u8]) -> RuleResult {
        let Some(first) = packet.first() else {
            return RuleResult::Break;
        };
        let header_len = usize::from(first & 0x0f) * 4;
        if first >> 4 != 4 || header_len < 20 || packet.len() < header_len {
            return RuleResult::Break;
        }
        let total_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
        if total_len < header_len || total_len > packet.len() {
            return RuleResult::Break;
        }
        let packet = &packet[..total_len];
        let Some(fragment_bytes) = packet.get(6..8) else {
            return RuleResult::Break;
        };
        let fragment = u16::from_be_bytes(fragment_bytes.try_into().unwrap()) & 0x1fff;
        if fragment != 0 {
            return if fragment == 1 {
                RuleResult::Verdict(NftRuleVerdict::Drop)
            } else {
                RuleResult::Break
            };
        }
        let Some(tcp) = packet.get(header_len..header_len + 20) else {
            return RuleResult::Verdict(NftRuleVerdict::Drop);
        };
        for (offset, range, invert) in [
            (0, self.source_ports, self.invert & 0x01 != 0),
            (2, self.dest_ports, self.invert & 0x02 != 0),
        ] {
            let port = u16::from_be_bytes([tcp[offset], tcp[offset + 1]]);
            if !((range[0] <= port && port <= range[1]) ^ invert) {
                return RuleResult::Break;
            }
        }
        if ((tcp[13] & self.flags_mask) == self.flags_match) == (self.invert & 0x04 != 0) {
            return RuleResult::Break;
        }
        if self.option != 0 {
            let tcp_header_len = usize::from(tcp[12] >> 4) * 4;
            if tcp_header_len < 20 {
                return RuleResult::Verdict(NftRuleVerdict::Drop);
            }
            let Some(options) = packet.get(header_len + 20..header_len + tcp_header_len) else {
                return RuleResult::Verdict(NftRuleVerdict::Drop);
            };
            let mut cursor = 0;
            let mut found = false;
            while cursor < options.len() {
                if options[cursor] == self.option {
                    found = true;
                    break;
                }
                if options[cursor] < 2 || cursor + 1 == options.len() {
                    cursor += 1;
                } else {
                    cursor += usize::from(options[cursor + 1].max(1));
                }
            }
            if found == (self.invert & 0x08 != 0) {
                return RuleResult::Break;
            }
        }
        RuleResult::Continue
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum NftPayloadBase {
    Network = 1,
    Transport = 2,
}

#[derive(Debug)]
pub(crate) enum NftBitwiseOperation {
    Bool { mask: Vec<u8>, xor: Vec<u8> },
    Lshift(u32),
    Rshift(u32),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftCmpOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftByteorderOp {
    NetworkToHost,
    HostToNetwork,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NftRangeOp {
    Eq,
    Neq,
}

#[derive(Debug)]
pub(crate) struct NftCounter {
    initial_bytes: u64,
    initial_packets: u64,
    shards: Vec<SpinLock<NftCounterPair>>,
}

#[derive(Debug, Default)]
struct NftCounterPair {
    bytes: u64,
    packets: u64,
}

impl NftCounter {
    fn new(bytes: u64, packets: u64) -> Result<Self, SystemError> {
        let mut shards = Vec::new();
        shards
            .try_reserve_exact(PerCpu::MAX_CPU_NUM as usize)
            .map_err(|_| SystemError::ENOMEM)?;
        for _ in 0..PerCpu::MAX_CPU_NUM {
            shards.push(SpinLock::new(NftCounterPair::default()));
        }
        Ok(Self {
            initial_bytes: bytes,
            initial_packets: packets,
            shards,
        })
    }

    fn count(&self, length: u64) {
        // Serialize the pair without introducing a global packet-path lock.
        let cpu = smp_get_processor_id().data() as usize;
        let mut pair = self.shards[cpu].lock_irqsave();
        pair.bytes = pair.bytes.wrapping_add(length);
        pair.packets = pair.packets.wrapping_add(1);
    }

    pub(crate) fn snapshot(&self) -> (u64, u64) {
        let mut bytes = self.initial_bytes;
        let mut packets = self.initial_packets;
        for shard in &self.shards {
            let pair = shard.lock_irqsave();
            bytes = bytes.wrapping_add(pair.bytes);
            packets = packets.wrapping_add(pair.packets);
        }
        (bytes, packets)
    }
}

fn data_register(register: u32, len: usize) -> Result<usize, SystemError> {
    let first = match register {
        1..=4 => (register as usize - 1) * 16,
        8..=23 => (register as usize - 8) * 4,
        _ => return Err(SystemError::ERANGE),
    };
    if len == 0 {
        return Err(SystemError::EINVAL);
    }
    if first.checked_add(len).is_none_or(|end| end > 64) {
        return Err(SystemError::ERANGE);
    }
    Ok(first)
}

impl NftChain {
    pub(crate) fn rules(&self) -> &[Arc<NftRule>] {
        &self.rules
    }

    fn clone_for_write(&self) -> Result<Arc<Self>, SystemError> {
        let mut rules = Vec::new();
        rules
            .try_reserve_exact(self.rules.len())
            .map_err(|_| SystemError::ENOMEM)?;
        rules.extend(self.rules.iter().cloned());
        Arc::try_new(Self {
            name: copy_bytes(&self.name)?,
            handle: self.handle,
            base: self.base,
            rules,
        })
        .map_err(|_| SystemError::ENOMEM)
    }
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, SystemError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| SystemError::ENOMEM)?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

fn allocated_set_name(table: &NftTable, template: &[u8]) -> Result<Vec<u8>, SystemError> {
    let Some(percent) = template.iter().position(|byte| *byte == b'%') else {
        return copy_bytes(template);
    };
    if template.get(percent..) != Some(b"%d") || percent + 20 >= 256 {
        return Err(SystemError::EINVAL);
    }
    for number in 0..=table.sets.len() {
        let mut digits = [0u8; 20];
        let mut remaining = number;
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        let mut name = Vec::new();
        name.try_reserve_exact(percent + digits.len() - start)
            .map_err(|_| SystemError::ENOMEM)?;
        name.extend_from_slice(&template[..percent]);
        name.extend_from_slice(&digits[start..]);
        if !table.sets.iter().any(|set| set.name == name) {
            return Ok(name);
        }
    }
    Err(SystemError::ENFILE)
}

fn verdict_target(verdict: NftRuleVerdict) -> Option<u64> {
    match verdict {
        NftRuleVerdict::Jump(target) | NftRuleVerdict::Goto(target) => Some(target),
        _ => None,
    }
}

impl NftTable {
    /// Static and map-supplied edges share one graph. A verdict map may be
    /// updated in a later transaction, so graph validation runs at commit.
    fn rule_targets<'a>(&'a self, rule: &'a NftRule) -> impl Iterator<Item = u64> + 'a {
        rule.expressions.iter().flat_map(move |expression| {
            let direct = match expression {
                NftExpression::Immediate(verdict) => verdict_target(*verdict),
                _ => None,
            };
            let map_elements = match expression {
                NftExpression::Lookup {
                    set_handle,
                    verdict_map: true,
                    ..
                } => self
                    .sets
                    .binary_search_by_key(set_handle, |set| set.handle)
                    .ok()
                    .map_or(&[][..], |index| self.sets[index].elements.as_slice()),
                _ => &[],
            };
            direct.into_iter().chain(
                map_elements
                    .iter()
                    .filter_map(|element| element.verdict.and_then(verdict_target)),
            )
        })
    }

    fn requires_route_lookup(&self) -> bool {
        self.chains
            .iter()
            .filter(|chain| chain.base.is_some())
            .any(|chain| self.chain_requires_route_lookup(chain, 0))
    }

    fn chain_requires_iface_names(&self, chain: &NftChain, depth: usize) -> bool {
        if depth >= 16 {
            return false;
        }
        chain.rules.iter().any(|rule| {
            rule.expressions.iter().any(|expression| {
                matches!(
                    expression,
                    NftExpression::Meta {
                        key: NftMetaKey::Iifname | NftMetaKey::Oifname,
                        ..
                    }
                )
            }) || self.rule_targets(rule).any(|target| {
                self.chain_index(target).is_some_and(|index| {
                    self.chain_requires_iface_names(&self.chains[index], depth + 1)
                })
            })
        })
    }

    fn chain_requires_route_lookup(&self, chain: &NftChain, depth: usize) -> bool {
        // The same transaction has already validated a maximum call depth of
        // 16, so this walk is bounded even for user-supplied chain graphs.
        if depth >= 16 {
            return false;
        }
        chain.rules.iter().any(|rule| {
            rule.expressions.iter().any(|expression| {
                matches!(
                    expression,
                    NftExpression::XtAddrtype(_) | NftExpression::FibDaddrType { .. }
                )
            }) || self.rule_targets(rule).any(|target| {
                self.chain_index(target).is_some_and(|index| {
                    self.chain_requires_route_lookup(&self.chains[index], depth + 1)
                })
            })
        })
    }

    fn chain_requires_redirect(&self, chain: &NftChain, depth: usize) -> bool {
        if depth >= 16 {
            return false;
        }
        chain.rules.iter().any(|rule| {
            rule.expressions
                .iter()
                .any(|expression| matches!(expression, NftExpression::Redirect { .. }))
                || self.rule_targets(rule).any(|target| {
                    self.chain_index(target).is_some_and(|index| {
                        self.chain_requires_redirect(&self.chains[index], depth + 1)
                    })
                })
        })
    }

    fn validate_reachable_chains(&self) -> Result<(), SystemError> {
        let mut visiting = Vec::new();
        visiting
            .try_reserve_exact(self.chains.len())
            .map_err(|_| SystemError::ENOMEM)?;
        visiting.resize(self.chains.len(), false);
        let mut depths = Vec::new();
        depths
            .try_reserve_exact(self.chains.len())
            .map_err(|_| SystemError::ENOMEM)?;
        depths.resize(self.chains.len(), None);
        for (index, chain) in self.chains.iter().enumerate() {
            if let Some(base) = chain.base {
                self.max_call_depth(index, 0, &mut visiting, &mut depths)?;
                self.validate_nat_chain(index, base, 0)?;
            }
        }
        Ok(())
    }

    fn validate_nat_chain(
        &self,
        index: usize,
        base: NftBaseChain,
        depth: u8,
    ) -> Result<(), SystemError> {
        if depth >= 16 {
            return Err(SystemError::EMLINK);
        }
        for rule in &self.chains[index].rules {
            for expression in &rule.expressions {
                let kind = match expression {
                    NftExpression::Nat { side, .. } => Some((*side, false)),
                    NftExpression::Masquerade { .. } => Some((NatManipSide::Source, true)),
                    NftExpression::Redirect { .. } => Some((NatManipSide::Destination, false)),
                    NftExpression::XtNatTarget { action, .. } => Some(match action {
                        NftNatAction::Dnat(_) => (NatManipSide::Destination, false),
                        NftNatAction::Snat(_) => (NatManipSide::Source, false),
                        NftNatAction::Masquerade { .. } => (NatManipSide::Source, true),
                    }),
                    _ => None,
                };
                if let Some((side, masquerade)) = kind {
                    if base.chain_type != NftChainType::Nat
                        || !match side {
                            NatManipSide::Destination => {
                                matches!(base.hook, NftIpv4Hook::PreRouting | NftIpv4Hook::LocalOut)
                            }
                            NatManipSide::Source => {
                                matches!(base.hook, NftIpv4Hook::LocalIn | NftIpv4Hook::PostRouting)
                            }
                        }
                        || (masquerade && base.hook != NftIpv4Hook::PostRouting)
                    {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                }
            }
            for target in self.rule_targets(rule) {
                let child = self.chain_index(target).ok_or(SystemError::ENOENT)?;
                self.validate_nat_chain(child, base, depth + 1)?;
            }
        }
        Ok(())
    }

    fn max_call_depth(
        &self,
        index: usize,
        level: u8,
        visiting: &mut [bool],
        depths: &mut [Option<u8>],
    ) -> Result<u8, SystemError> {
        // Reject before descending: a malicious long chain graph must not
        // consume an unbounded kernel call stack while validating a batch.
        if level >= 16 {
            return Err(SystemError::EMLINK);
        }
        if visiting[index] {
            // Linux follows the cycle until its 16-level validation limit.
            return Err(SystemError::EMLINK);
        }
        if let Some(depth) = depths[index] {
            if level + depth >= 16 {
                return Err(SystemError::EMLINK);
            }
            return Ok(depth);
        }
        visiting[index] = true;
        let mut depth = 0u8;
        for rule in &self.chains[index].rules {
            for target in self.rule_targets(rule) {
                let target_index = self.chain_index(target).ok_or(SystemError::ENOENT)?;
                if self.chains[target_index].base.is_some() {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                let child_depth = self.max_call_depth(target_index, level + 1, visiting, depths)?;
                depth = depth.max(child_depth.checked_add(1).ok_or(SystemError::EMLINK)?);
                if depth >= 16 {
                    return Err(SystemError::EMLINK);
                }
            }
        }
        visiting[index] = false;
        depths[index] = Some(depth);
        Ok(depth)
    }

    pub(crate) fn chain_use_count(&self, chain: &NftChain) -> u32 {
        let references = chain
            .rules
            .len()
            .saturating_add(self.inbound_references(chain.handle));
        u32::try_from(references).unwrap_or(u32::MAX)
    }

    fn chain_index(&self, handle: u64) -> Option<usize> {
        // Handles increase on creation and deletion preserves the order.
        self.chains
            .binary_search_by_key(&handle, |chain| chain.handle)
            .ok()
    }

    fn inbound_references(&self, handle: u64) -> usize {
        let rules = self
            .chains
            .iter()
            .flat_map(|chain| chain.rules.iter())
            .filter(|rule| {
                matches!(
                    rule.verdict(),
                    NftRuleVerdict::Jump(target) | NftRuleVerdict::Goto(target)
                        if target == handle
                )
            })
            .count();
        let maps = self
            .sets
            .iter()
            .flat_map(|set| set.elements.iter())
            .filter(|element| element.verdict.and_then(verdict_target) == Some(handle))
            .count();
        rules.saturating_add(maps)
    }

    fn evaluate_base_chain(&self, base_handle: u64, packet: &NftPacket<'_>) -> NftVerdict {
        match self.evaluate_base_chain_result(base_handle, packet) {
            ChainResult::Verdict(verdict) => verdict,
            // A NAT action at a filter hook is invalid and must fail closed.
            ChainResult::Nat(_) => NftVerdict::Drop,
        }
    }

    fn evaluate_base_chain_result(&self, base_handle: u64, packet: &NftPacket<'_>) -> ChainResult {
        const JUMP_STACK_SIZE: usize = 16;
        let Some(mut chain_index) = self.chain_index(base_handle) else {
            // The compiled hook always comes from this immutable table.
            return ChainResult::Verdict(NftVerdict::Drop);
        };
        let policy = self.chains[chain_index]
            .base
            .expect("only base chains are hooked")
            .policy;
        let mut rule_index = 0;
        let mut stack = [(0usize, 0usize); JUMP_STACK_SIZE];
        let mut depth = 0;
        loop {
            if let Some(rule) = self.chains[chain_index].rules.get(rule_index) {
                rule_index += 1;
                match rule.evaluate(packet, &self.sets) {
                    RuleResult::Break | RuleResult::Continue => continue,
                    RuleResult::Nat(action) => return ChainResult::Nat(action),
                    RuleResult::Verdict(verdict) => match verdict {
                        NftRuleVerdict::Accept => return ChainResult::Verdict(NftVerdict::Accept),
                        NftRuleVerdict::Drop => return ChainResult::Verdict(NftVerdict::Drop),
                        NftRuleVerdict::Continue => continue,
                        NftRuleVerdict::Return => {}
                        NftRuleVerdict::Jump(target) | NftRuleVerdict::Goto(target) => {
                            let jump = matches!(verdict, NftRuleVerdict::Jump(_));
                            let Some(target_index) = self.chain_index(target) else {
                                return ChainResult::Verdict(NftVerdict::Drop);
                            };
                            if jump {
                                if depth == JUMP_STACK_SIZE {
                                    return ChainResult::Verdict(NftVerdict::Drop);
                                }
                                stack[depth] = (chain_index, rule_index);
                                depth += 1;
                            }
                            chain_index = target_index;
                            rule_index = 0;
                            continue;
                        }
                    },
                }
            }
            if depth == 0 {
                return ChainResult::Verdict(policy);
            }
            depth -= 1;
            (chain_index, rule_index) = stack[depth];
        }
    }

    fn clone_for_write(&self) -> Result<Arc<Self>, SystemError> {
        let mut chains = Vec::new();
        chains
            .try_reserve_exact(self.chains.len())
            .map_err(|_| SystemError::ENOMEM)?;
        chains.extend(self.chains.iter().cloned());
        let mut sets = Vec::new();
        sets.try_reserve_exact(self.sets.len())
            .map_err(|_| SystemError::ENOMEM)?;
        sets.extend(self.sets.iter().cloned());
        Arc::try_new(Self {
            family: self.family,
            name: copy_bytes(&self.name)?,
            handle: self.handle,
            flags: self.flags,
            userdata: copy_bytes(&self.userdata)?,
            use_count: self.use_count,
            chains,
            sets,
            next_handle: self.next_handle,
        })
        .map_err(|_| SystemError::ENOMEM)
    }
}

/// One immutable hook program is published with the ruleset snapshot. The
/// fixed conntrack step is ordered with user chains, not run before raw chains.
#[derive(Debug)]
struct HookProgram {
    steps: Vec<HookStep>,
}

#[derive(Debug)]
enum HookStep {
    Conntrack {
        registration: u64,
    },
    FilterChain {
        table: Arc<NftTable>,
        chain: Arc<NftChain>,
    },
    NatOuter {
        side: NatManipSide,
        registration: u64,
        chains: Vec<(Arc<NftTable>, Arc<NftChain>)>,
    },
}

impl HookProgram {
    fn empty() -> Self {
        Self { steps: Vec::new() }
    }

    fn is_empty(&self) -> bool {
        !self.steps.iter().any(|step| {
            matches!(
                step,
                HookStep::FilterChain { .. } | HookStep::NatOuter { .. }
            )
        })
    }

    fn requires_iface_names(&self) -> bool {
        self.steps.iter().any(|step| match step {
            HookStep::FilterChain { table, chain } => table.chain_requires_iface_names(chain, 0),
            HookStep::NatOuter { chains, .. } => chains
                .iter()
                .any(|(table, chain)| table.chain_requires_iface_names(chain, 0)),
            HookStep::Conntrack { .. } => false,
        })
    }

    fn requires_route_lookup(&self) -> bool {
        self.steps.iter().any(|step| match step {
            HookStep::FilterChain { table, chain } => table.chain_requires_route_lookup(chain, 0),
            HookStep::NatOuter { chains, .. } => chains
                .iter()
                .any(|(table, chain)| table.chain_requires_route_lookup(chain, 0)),
            HookStep::Conntrack { .. } => false,
        })
    }

    fn evaluate(&self, packet: &NftPacket<'_>) -> NftVerdict {
        self.evaluate_with_tracking(packet, || true)
    }

    fn evaluate_with_tracking(
        &self,
        packet: &NftPacket<'_>,
        mut track: impl FnMut() -> bool,
    ) -> NftVerdict {
        for step in &self.steps {
            match step {
                HookStep::Conntrack { .. } if !track() => return NftVerdict::Drop,
                HookStep::Conntrack { .. } => {}
                // A NAT hook must use the mutable evaluator. Never return an
                // apparent ACCEPT when its packet rewrite was skipped.
                HookStep::NatOuter { .. } => return NftVerdict::Drop,
                HookStep::FilterChain { table, chain }
                    if table.evaluate_base_chain(chain.handle, packet) == NftVerdict::Drop =>
                {
                    return NftVerdict::Drop;
                }
                // ACCEPT terminates only this chain; subsequent base chains run.
                HookStep::FilterChain { .. } => {}
            }
        }
        NftVerdict::Accept
    }

    fn evaluate_with_nat(
        &self,
        packet: NftNatPacket<'_>,
        mut track: impl FnMut(&[u8]) -> bool,
        mut nat: impl FnMut(&mut [u8], NftNatEvent) -> Result<NftNatProgress, SystemError>,
    ) -> Result<bool, SystemError> {
        let NftNatPacket {
            bytes,
            conntrack,
            mark,
            redirect_address,
            iifname,
            oifname,
            ipv4_addr_type,
            ipv6_local_destination,
        } = packet;
        for step in &self.steps {
            match step {
                HookStep::Conntrack { .. } => {
                    if !track(bytes) {
                        return Ok(false);
                    }
                }
                HookStep::FilterChain { table, chain } => {
                    let packet = if let Some(addr_type) = ipv4_addr_type {
                        NftPacket::new(bytes, addr_type)
                    } else {
                        NftPacket::new_ipv6(bytes)
                    }
                    .with_conntrack_cell(conntrack)
                    .with_interface_names(iifname, oifname);
                    let packet = if let Some(mark) = mark {
                        packet.with_mark(mark)
                    } else {
                        packet
                    };
                    let packet = if let Some(resolve) = redirect_address {
                        packet.with_redirect_address(resolve)
                    } else {
                        packet
                    };
                    let packet = if let Some(lookup) = ipv6_local_destination {
                        packet.with_ipv6_local_destination(lookup)
                    } else {
                        packet
                    };
                    if table.evaluate_base_chain(chain.handle, &packet) == NftVerdict::Drop {
                        return Ok(false);
                    }
                }
                HookStep::NatOuter { side, chains, .. } => {
                    let progress = nat(bytes, NftNatEvent::Begin(*side))?;
                    if progress == NftNatProgress::Continue {
                        for (table, chain) in chains {
                            let packet = if let Some(addr_type) = ipv4_addr_type {
                                NftPacket::new(bytes, addr_type)
                            } else {
                                NftPacket::new_ipv6(bytes)
                            }
                            .with_conntrack_cell(conntrack)
                            .with_interface_names(iifname, oifname);
                            let packet = if let Some(mark) = mark {
                                packet.with_mark(mark)
                            } else {
                                packet
                            };
                            let packet = if let Some(resolve) = redirect_address {
                                packet.with_redirect_address(resolve)
                            } else {
                                packet
                            };
                            let packet = if let Some(lookup) = ipv6_local_destination {
                                packet.with_ipv6_local_destination(lookup)
                            } else {
                                packet
                            };
                            match table.evaluate_base_chain_result(chain.handle, &packet) {
                                ChainResult::Verdict(NftVerdict::Drop) => return Ok(false),
                                ChainResult::Verdict(NftVerdict::Accept) => {}
                                ChainResult::Nat(action) => {
                                    if nat(bytes, NftNatEvent::Rule(action))?
                                        == NftNatProgress::SkipRules
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    nat(bytes, NftNatEvent::Finish(*side))?;
                }
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod hook_program_tests {
    use super::*;

    #[test]
    fn ipv6_xt_addrtype_local_uses_supplied_fib_view_and_final_destination() {
        let mut info = [0u8; 8];
        info[2..4].copy_from_slice(&(1u16 << crate::net::route::RTN_LOCAL).to_ne_bytes());
        let matcher = NftXtAddrtype::from_info(&info).unwrap();
        let mut bytes = [0u8; 40];
        bytes[0] = 0x60;
        bytes[24..40].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(matcher.evaluate(&NftPacket::new_ipv6(&bytes)), None);
        let rule = NftRule {
            handle: 1,
            expressions: alloc::vec![NftExpression::XtAddrtype(matcher)],
        };
        assert!(matches!(
            rule.evaluate(&NftPacket::new_ipv6(&bytes), &[]),
            RuleResult::Verdict(NftRuleVerdict::Drop)
        ));
        let local = |address: Ipv6Address| address == Ipv6Address::LOCALHOST;
        let view = NftPacket::new_ipv6(&bytes).with_ipv6_local_destination(&local);
        assert_eq!(matcher.evaluate(&view), Some(true));
        let nonlocal = |_address: Ipv6Address| false;
        let view = NftPacket::new_ipv6(&bytes).with_ipv6_local_destination(&nonlocal);
        assert_eq!(matcher.evaluate(&view), Some(false));
        bytes[39] = 2;
        let view = NftPacket::new_ipv6(&bytes).with_ipv6_local_destination(&local);
        assert_eq!(matcher.evaluate(&view), Some(false));
    }

    #[test]
    fn ipv6_route_view_is_requested_only_for_reachable_addrtype_hook() {
        let mut table = table_with_chain(10, 1, 0, 1, NftVerdict::Accept, None);
        let mutable = Arc::get_mut(&mut table).unwrap();
        let chain = Arc::get_mut(&mut mutable.chains[0]).unwrap();
        let mut info = [0u8; 8];
        info[2..4].copy_from_slice(&(1u16 << crate::net::route::RTN_LOCAL).to_ne_bytes());
        chain.rules.push(Arc::new(NftRule {
            handle: 2,
            expressions: alloc::vec![NftExpression::XtAddrtype(
                NftXtAddrtype::from_info(&info).unwrap(),
            )],
        }));
        let mut ruleset = snapshot(alloc::vec![table]);
        ruleset.compile_ipv4_hooks().unwrap();
        assert!(ruleset.ipv6_hook_requires_local_destination(NftIpv4Hook::PreRouting));
        assert!(!ruleset.ipv6_hook_requires_local_destination(NftIpv4Hook::LocalOut));
        assert!(!ruleset.ipv4_hook_requires_route_lookup(NftIpv4Hook::PreRouting));
        assert!(!ruleset.ipv4_hook_requires_route_lookup(NftIpv4Hook::LocalOut));
    }

    #[test]
    fn ipv4_route_view_is_scoped_to_family_hook_and_reachable_jump() {
        let mut table = table_with_chain(2, 1, 0, 1, NftVerdict::Accept, None);
        let mutable = Arc::get_mut(&mut table).unwrap();
        let base = Arc::get_mut(&mut mutable.chains[0]).unwrap();
        base.base.as_mut().unwrap().hook = NftIpv4Hook::LocalIn;
        base.rules.push(Arc::new(NftRule {
            handle: 3,
            expressions: alloc::vec![NftExpression::Immediate(NftRuleVerdict::Jump(2))],
        }));
        let mut info = [0u8; 8];
        info[2..4].copy_from_slice(&(1u16 << crate::net::route::RTN_LOCAL).to_ne_bytes());
        mutable.chains.push(Arc::new(NftChain {
            name: b"target".to_vec(),
            handle: 2,
            base: None,
            rules: alloc::vec![Arc::new(NftRule {
                handle: 4,
                expressions: alloc::vec![NftExpression::XtAddrtype(
                    NftXtAddrtype::from_info(&info).unwrap(),
                )],
            })],
        }));
        let mut ruleset = snapshot(alloc::vec![table]);
        ruleset.compile_ipv4_hooks().unwrap();
        assert!(ruleset.ipv4_hook_requires_route_lookup(NftIpv4Hook::LocalIn));
        assert!(!ruleset.ipv4_hook_requires_route_lookup(NftIpv4Hook::PreRouting));
        assert!(!ruleset.ipv4_hook_requires_route_lookup(NftIpv4Hook::LocalOut));
        assert!(!ruleset.ipv6_hook_requires_local_destination(NftIpv4Hook::LocalIn));
    }

    fn table_with_chain(
        family: u8,
        handle: u64,
        priority: i32,
        hook_order: u64,
        policy: NftVerdict,
        rule: Option<NftRuleVerdict>,
    ) -> Arc<NftTable> {
        let rules = rule.map_or_else(Vec::new, |verdict| {
            alloc::vec![Arc::new(NftRule {
                handle: 1,
                expressions: alloc::vec![NftExpression::Immediate(verdict)],
            })]
        });
        Arc::new(NftTable {
            family,
            name: alloc::vec![family],
            handle,
            flags: 0,
            userdata: Vec::new(),
            use_count: 1,
            chains: alloc::vec![Arc::new(NftChain {
                name: alloc::vec![b'c'],
                handle: 1,
                base: Some(NftBaseChain {
                    hook_order,
                    hook: NftIpv4Hook::PreRouting,
                    priority,
                    policy,
                    chain_type: NftChainType::Filter,
                }),
                rules,
            })],
            sets: Vec::new(),
            next_handle: 2,
        })
    }

    fn snapshot(tables: Vec<Arc<NftTable>>) -> RulesetSnapshot {
        RulesetSnapshot {
            generation: 1,
            tables,
            next_table_handle: 4,
            next_hook_order: 4,
            conntrack_hook_order: [Some(0), Some(0)],
            nat_outer_hook_order: [None; 2],
            requires_route_lookup: false,
            requires_iface_names: false,
            iface_name_hooks: [false; NftIpv4Hook::COUNT],
            ipv4_route_lookup_hooks: [false; NftIpv4Hook::COUNT],
            ipv6_iface_name_hooks: [false; NftIpv4Hook::COUNT],
            ipv6_local_destination_hooks: [false; NftIpv4Hook::COUNT],
            ipv4_hooks: core::array::from_fn(|_| HookProgram::empty()),
            ipv6_hooks: core::array::from_fn(|_| HookProgram::empty()),
        }
    }

    #[test]
    fn ct_activation_follows_final_rule_families() {
        let mut ipv4 = table_with_chain(2, 1, 0, 1, NftVerdict::Accept, None);
        let table = Arc::get_mut(&mut ipv4).unwrap();
        let chain = Arc::get_mut(&mut table.chains[0]).unwrap();
        chain.rules.push(Arc::new(NftRule {
            handle: 2,
            expressions: alloc::vec![NftExpression::CtState { dreg: 0 }],
        }));
        let mut candidate = snapshot(alloc::vec![ipv4]);
        candidate.conntrack_hook_order = [None; 2];
        assert_eq!(candidate.conntrack_consumer_families(), [true, false]);
        let mut inet = table_with_chain(1, 2, 0, 2, NftVerdict::Accept, None);
        let table = Arc::get_mut(&mut inet).unwrap();
        let chain = Arc::get_mut(&mut table.chains[0]).unwrap();
        chain.rules.push(Arc::new(NftRule {
            handle: 2,
            expressions: alloc::vec![NftExpression::CtState { dreg: 0 }],
        }));
        candidate.tables.push(inet);
        assert_eq!(candidate.conntrack_consumer_families(), [true, true]);
        candidate.tables.clear();
        assert_eq!(candidate.conntrack_consumer_families(), [false, false]);
    }

    #[test]
    fn last_ct_consumer_removes_new_snapshot_hooks_but_not_old_snapshot() {
        let old = snapshot(Vec::new());
        let mut next = snapshot(Vec::new());
        assert!(old.conntrack_registered(IpVersion::Ipv6));
        assert_eq!(
            next.reconcile_conntrack_hooks([None; 2]).unwrap(),
            [false, false]
        );
        next.compile_ipv4_hooks().unwrap();
        assert!(!next.conntrack_registered(IpVersion::Ipv4));
        assert!(!next.conntrack_registered(IpVersion::Ipv6));
        assert!(!next.has_ipv6_hook(NftIpv4Hook::PreRouting));
        assert!(!next.has_output_hook());
        assert!(old.conntrack_registered(IpVersion::Ipv6));
    }

    #[test]
    fn nat_base_chain_remains_a_conntrack_consumer() {
        let mut nat = table_with_chain(10, 1, 100, 1, NftVerdict::Accept, None);
        Arc::get_mut(&mut Arc::get_mut(&mut nat).unwrap().chains[0])
            .unwrap()
            .base
            .as_mut()
            .unwrap()
            .chain_type = NftChainType::Nat;
        let mut next = snapshot(alloc::vec![nat]);
        assert_eq!(next.conntrack_consumer_families(), [false, true]);
        next.reconcile_conntrack_hooks([None; 2]).unwrap();
        assert!(!next.conntrack_registered(IpVersion::Ipv4));
        assert!(next.conntrack_registered(IpVersion::Ipv6));
    }

    #[test]
    fn nat_outer_keeps_registration_until_last_base_chain_disappears() {
        let mut nat = table_with_chain(2, 1, -110, 1, NftVerdict::Accept, None);
        Arc::get_mut(&mut Arc::get_mut(&mut nat).unwrap().chains[0])
            .unwrap()
            .base
            .as_mut()
            .unwrap()
            .chain_type = NftChainType::Nat;
        let mut candidate = snapshot(alloc::vec![nat]);
        candidate.nat_outer_hook_order[0] = Some(7);
        candidate.reconcile_nat_hooks([None; 2]).unwrap();
        assert_eq!(candidate.nat_outer_hook_order[0], Some(7));
        let compiled = candidate
            .compile_hook(NftIpv4Hook::PreRouting, &[2])
            .unwrap();
        assert!(compiled.steps.iter().any(|step| matches!(
            step,
            HookStep::NatOuter {
                registration: 7,
                ..
            }
        )));
        // A PRE-only DNAT chain still installs POST's fixed outer hook to
        // reverse the translated destination on reply packets.
        let reply = candidate
            .compile_hook(NftIpv4Hook::PostRouting, &[2])
            .unwrap();
        assert!(reply.steps.iter().any(|step| matches!(
            step,
            HookStep::NatOuter {
                side: NatManipSide::Source,
                chains,
                ..
            } if chains.is_empty()
        )));
        assert!(candidate.hook_may_nat(IpVersion::Ipv4, NftIpv4Hook::PostRouting));
        candidate.tables.clear();
        candidate.reconcile_nat_hooks([None; 2]).unwrap();
        assert_eq!(candidate.nat_outer_hook_order[0], None);
        assert!(!candidate.hook_may_nat(IpVersion::Ipv4, NftIpv4Hook::PostRouting));
    }

    #[test]
    fn nat_outer_events_run_at_fixed_priority_before_later_filter() {
        let mut nat = table_with_chain(2, 1, 50, 1, NftVerdict::Accept, None);
        let nat_table = Arc::get_mut(&mut nat).unwrap();
        let nat_chain = Arc::get_mut(&mut nat_table.chains[0]).unwrap();
        nat_chain.base.as_mut().unwrap().chain_type = NftChainType::Nat;
        nat_chain.rules.push(Arc::new(NftRule {
            handle: 2,
            expressions: alloc::vec![NftExpression::XtNatTarget {
                name: b"DNAT",
                revision: 2,
                info: Vec::new(),
                action: NftNatAction::Dnat(
                    CtNatRequest::new(Some(CtAddress::V4([127, 0, 0, 1])), None).unwrap()
                ),
            }],
        }));
        let filter = table_with_chain(2, 2, 0, 2, NftVerdict::Drop, None);
        let mut ruleset = snapshot(alloc::vec![nat, filter]);
        ruleset.nat_outer_hook_order[0] = Some(1);
        let program = ruleset.compile_hook(NftIpv4Hook::PreRouting, &[2]).unwrap();
        assert!(matches!(program.steps[1], HookStep::NatOuter { .. }));
        let mut packet = [0x45u8; 20];
        let conntrack = RefCell::new(None);
        let addr_type = |_| 0;
        let mut events = Vec::new();
        let accepted = program
            .evaluate_with_nat(
                NftNatPacket::new_ipv4(&mut packet, &conntrack, [0; 16], [0; 16], &addr_type),
                |_| true,
                |_, event| {
                    events.push(event);
                    Ok(if matches!(event, NftNatEvent::Rule(_)) {
                        NftNatProgress::SkipRules
                    } else {
                        NftNatProgress::Continue
                    })
                },
            )
            .unwrap();
        assert!(!accepted); // Later filter chain still runs after NAT.
        assert!(matches!(
            events.as_slice(),
            [
                NftNatEvent::Begin(NatManipSide::Destination),
                NftNatEvent::Rule(NftNatAction::Dnat(_)),
                NftNatEvent::Finish(NatManipSide::Destination),
            ]
        ));
    }

    #[test]
    fn redirect_requires_a_real_address_and_runs_as_dnat() {
        let rule = NftRule {
            handle: 1,
            expressions: alloc::vec![NftExpression::Redirect {
                flags: 0,
                port_min: None,
                port_max: None,
            }],
        };
        let packet = [0x45u8; 20];
        let addr_type = |_| 0;
        assert!(matches!(
            rule.evaluate(&NftPacket::new(&packet, &addr_type), &[]),
            RuleResult::Verdict(NftRuleVerdict::Drop)
        ));
        let resolve = |_packet: &[u8]| Some(CtAddress::V4([127, 0, 0, 1]));
        assert!(matches!(
            rule.evaluate(
                &NftPacket::new(&packet, &addr_type).with_redirect_address(&resolve),
                &[]
            ),
            RuleResult::Nat(NftNatAction::Dnat(_))
        ));
    }

    #[test]
    fn redirect_rejects_unsupported_flags_and_non_destination_nat_hooks() {
        let state = NftNamespaceState::new();
        let mut transaction = state.transaction(0).unwrap();
        transaction
            .new_table(2, b"nat", 0, &[], false, false)
            .unwrap();
        transaction
            .new_ip_chain(
                2,
                b"nat",
                b"pre",
                Some((
                    NftIpv4Hook::PreRouting,
                    -100,
                    NftVerdict::Accept,
                    NftChainType::Nat,
                )),
                false,
                false,
            )
            .unwrap();
        assert!(matches!(
            transaction.new_ip_rule(
                2,
                b"nat",
                b"pre",
                &[NftExpressionInput::Redirect {
                    flags: Some(4),
                    proto_min_reg: None,
                    proto_max_reg: None,
                }],
                true,
                None,
            ),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
        assert!(matches!(
            transaction.new_ip_rule(
                2,
                b"nat",
                b"pre",
                &[NftExpressionInput::Redirect {
                    flags: Some(2),
                    proto_min_reg: Some(8),
                    proto_max_reg: None,
                }],
                true,
                None,
            ),
            Err(SystemError::ENODATA)
        ));
        transaction
            .new_ip_rule(
                2,
                b"nat",
                b"pre",
                &[NftExpressionInput::Redirect {
                    flags: None,
                    proto_min_reg: None,
                    proto_max_reg: None,
                }],
                true,
                None,
            )
            .unwrap();
        transaction.candidate.tables[0]
            .validate_reachable_chains()
            .unwrap();
        transaction
            .new_ip_chain(
                2,
                b"nat",
                b"post",
                Some((
                    NftIpv4Hook::PostRouting,
                    100,
                    NftVerdict::Accept,
                    NftChainType::Nat,
                )),
                false,
                false,
            )
            .unwrap();
        transaction
            .new_ip_rule(
                2,
                b"nat",
                b"post",
                &[NftExpressionInput::Redirect {
                    flags: None,
                    proto_min_reg: None,
                    proto_max_reg: None,
                }],
                true,
                None,
            )
            .unwrap();
        assert!(matches!(
            transaction.candidate.tables[0].validate_reachable_chains(),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
    }

    #[test]
    fn xt_nat_targets_reject_ranges_and_unsupported_flags() {
        let mut dnat4 = [0u8; 24];
        dnat4[..4].copy_from_slice(&1u32.to_ne_bytes());
        dnat4[4..8].copy_from_slice(&1u32.to_ne_bytes());
        dnat4[8..12].copy_from_slice(&[127, 0, 0, 1]);
        dnat4[12..16].copy_from_slice(&[127, 0, 0, 1]);
        assert!(matches!(
            parse_xt_nat_target(2, b"DNAT", 0, &dnat4),
            Ok(NftNatAction::Dnat(_))
        ));
        assert!(matches!(
            parse_xt_nat_target(2, b"SNAT", 0, &dnat4),
            Ok(NftNatAction::Snat(_))
        ));
        dnat4[12] = 2;
        assert!(matches!(
            parse_xt_nat_target(2, b"DNAT", 0, &dnat4),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));

        let mut masq = [0u8; 24];
        masq[..4].copy_from_slice(&1u32.to_ne_bytes());
        assert!(matches!(
            parse_xt_nat_target(2, b"MASQUERADE", 0, &masq),
            Ok(NftNatAction::Masquerade { ports: None })
        ));
        masq[4..8].copy_from_slice(&4u32.to_ne_bytes());
        assert!(matches!(
            parse_xt_nat_target(2, b"MASQUERADE", 0, &masq),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));

        let mut dnat = [0u8; 48];
        dnat[..4].copy_from_slice(&1u32.to_ne_bytes());
        dnat[4..20].copy_from_slice(&[1u8; 16]);
        dnat[20..36].copy_from_slice(&[1u8; 16]);
        assert!(matches!(
            parse_xt_nat_target(10, b"DNAT", 2, &dnat),
            Ok(NftNatAction::Dnat(_))
        ));
        dnat[20] = 2;
        assert!(matches!(
            parse_xt_nat_target(10, b"DNAT", 2, &dnat),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
    }

    #[test]
    fn nat_action_is_not_reachable_from_filter_base_chain() {
        let mut table = table_with_chain(2, 1, 0, 1, NftVerdict::Accept, None);
        Arc::get_mut(&mut Arc::get_mut(&mut table).unwrap().chains[0])
            .unwrap()
            .rules
            .push(Arc::new(NftRule {
                handle: 2,
                expressions: alloc::vec![NftExpression::Masquerade {
                    port_min: None,
                    port_max: None,
                }],
            }));
        assert!(matches!(
            table.validate_reachable_chains(),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        ));
        let base = &mut Arc::get_mut(&mut Arc::get_mut(&mut table).unwrap().chains[0])
            .unwrap()
            .base
            .as_mut()
            .unwrap();
        base.chain_type = NftChainType::Nat;
        base.hook = NftIpv4Hook::PostRouting;
        assert!(table.validate_reachable_chains().is_ok());
    }

    #[test]
    fn tracking_registration_keeps_local_output_on_filtered_path() {
        let mut ruleset = snapshot(Vec::new());
        ruleset.conntrack_hook_order = [None; 2];
        assert!(!ruleset.has_output_hook());
        ruleset.conntrack_hook_order[0] = Some(1);
        assert!(ruleset.has_ipv4_hook(NftIpv4Hook::LocalOut));
        assert!(!ruleset.has_ipv6_hook(NftIpv4Hook::LocalOut));
        assert!(!ruleset.has_ipv4_hook(NftIpv4Hook::PostRouting));
        assert!(ruleset.has_output_hook());
    }

    #[test]
    fn ipv6_egress_chain_requires_the_routed_output_owner() {
        let mut table = table_with_chain(10, 1, 0, 1, NftVerdict::Drop, None);
        Arc::get_mut(&mut Arc::get_mut(&mut table).unwrap().chains[0])
            .unwrap()
            .base
            .as_mut()
            .unwrap()
            .hook = NftIpv4Hook::LocalOut;
        let mut ruleset = snapshot(alloc::vec![table]);
        ruleset.compile_ipv4_hooks().unwrap();
        assert!(!ruleset.has_ipv4_hook(NftIpv4Hook::LocalOut));
        assert!(ruleset.has_ipv6_hook(NftIpv4Hook::LocalOut));
        assert!(ruleset.has_output_hook());
    }

    #[test]
    fn filter_steps_keep_priority_and_newer_equal_priority_first() {
        let early = table_with_chain(2, 1, -300, 1, NftVerdict::Accept, None);
        let older = table_with_chain(2, 2, 0, 2, NftVerdict::Accept, None);
        let newer = table_with_chain(1, 3, 0, 3, NftVerdict::Accept, None);
        let ruleset = snapshot(alloc::vec![older, newer, early]);
        let program = ruleset
            .compile_hook(NftIpv4Hook::PreRouting, &[1, 2])
            .unwrap();
        let handles: Vec<u64> = program
            .steps
            .iter()
            .map(|step| match step {
                HookStep::FilterChain { table, .. } => table.handle,
                HookStep::Conntrack { .. } => 0,
                HookStep::NatOuter { .. } => u64::MAX,
            })
            .collect();
        assert_eq!(handles, alloc::vec![1, 3, 2, 0]);
    }

    #[test]
    fn accept_in_one_base_chain_does_not_skip_a_later_drop() {
        let accepting =
            table_with_chain(2, 1, 0, 1, NftVerdict::Drop, Some(NftRuleVerdict::Accept));
        let dropping = table_with_chain(2, 2, 10, 2, NftVerdict::Drop, None);
        let ruleset = snapshot(alloc::vec![accepting, dropping]);
        let program = ruleset
            .compile_hook(NftIpv4Hook::PreRouting, &[1, 2])
            .unwrap();
        let addr_type = |_| 1;
        assert_eq!(
            program.evaluate(&NftPacket::new(&[0x45], &addr_type)),
            NftVerdict::Drop
        );
        assert!(ruleset
            .compile_hook(NftIpv4Hook::PreRouting, &[1, 10])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn conntrack_runs_after_raw_and_newer_equal_priority_chains() {
        let raw_drop = table_with_chain(2, 1, -300, 1, NftVerdict::Drop, None);
        let same_priority_drop = table_with_chain(2, 2, -200, 2, NftVerdict::Drop, None);
        let addr_type = |_| 1;
        let packet = NftPacket::new(&[0x45], &addr_type);
        let tracked = core::cell::Cell::new(0);

        let raw_program = snapshot(alloc::vec![raw_drop])
            .compile_hook(NftIpv4Hook::PreRouting, &[2])
            .unwrap();
        assert_eq!(
            raw_program.evaluate_with_tracking(&packet, || {
                tracked.set(tracked.get() + 1);
                true
            }),
            NftVerdict::Drop
        );
        assert_eq!(tracked.get(), 0);

        let equal_program = snapshot(alloc::vec![same_priority_drop])
            .compile_hook(NftIpv4Hook::PreRouting, &[2])
            .unwrap();
        assert_eq!(
            equal_program.evaluate_with_tracking(&packet, || {
                tracked.set(tracked.get() + 1);
                true
            }),
            NftVerdict::Drop
        );
        assert_eq!(tracked.get(), 0);

        let no_chains = snapshot(Vec::new())
            .compile_hook(NftIpv4Hook::PreRouting, &[2])
            .unwrap();
        assert!(no_chains.is_empty());
        assert_eq!(
            no_chains.evaluate_with_tracking(&packet, || {
                tracked.set(tracked.get() + 1);
                true
            }),
            NftVerdict::Accept
        );
        assert_eq!(tracked.get(), 1);
    }

    #[test]
    fn conntrack_equal_priority_uses_activation_registration_order() {
        let old = table_with_chain(2, 1, -200, 1, NftVerdict::Accept, None);
        let new = table_with_chain(2, 2, -200, 3, NftVerdict::Accept, None);
        let mut ruleset = snapshot(alloc::vec![old, new]);
        ruleset.conntrack_hook_order[0] = Some(2);
        let program = ruleset.compile_hook(NftIpv4Hook::PreRouting, &[2]).unwrap();
        let registration: Vec<u64> = program
            .steps
            .iter()
            .map(|step| match step {
                HookStep::Conntrack { registration } => *registration,
                HookStep::FilterChain { chain, .. } => chain.base.unwrap().hook_order,
                HookStep::NatOuter { registration, .. } => *registration,
            })
            .collect();
        assert_eq!(registration, alloc::vec![3, 2, 1]);

        ruleset.conntrack_hook_order[0] = None;
        let without_tracking = ruleset.compile_hook(NftIpv4Hook::PreRouting, &[2]).unwrap();
        assert_eq!(without_tracking.steps.len(), 2);

        let ipv6 = table_with_chain(10, 3, -200, 4, NftVerdict::Accept, None);
        ruleset.tables.push(ipv6);
        ruleset.conntrack_hook_order[1] = Some(5);
        let ipv6_program = ruleset
            .compile_hook(NftIpv4Hook::PreRouting, &[1, 10])
            .unwrap();
        assert!(matches!(
            ipv6_program.steps.first(),
            Some(HookStep::Conntrack { registration: 5 })
        ));
    }
}

#[derive(Debug)]
pub(crate) struct RulesetSnapshot {
    pub(crate) generation: u32,
    pub(crate) tables: Vec<Arc<NftTable>>,
    pub(crate) next_table_handle: u64,
    next_hook_order: u64,
    /// Independent IPv4/IPv6 registration sequences for this snapshot.
    /// The last consumer removes its family's hook; a later rule registers
    /// anew with a later sequence, as in Linux.
    conntrack_hook_order: [Option<u64>; 2],
    /// Linux registers all four fixed NAT outer hooks for a protocol when
    /// its first NAT base chain appears, retaining their registration order
    /// until the last base chain for that protocol is removed.
    nat_outer_hook_order: [Option<u64>; 2],
    requires_route_lookup: bool,
    requires_iface_names: bool,
    iface_name_hooks: [bool; NftIpv4Hook::COUNT],
    ipv4_route_lookup_hooks: [bool; NftIpv4Hook::COUNT],
    ipv6_iface_name_hooks: [bool; NftIpv4Hook::COUNT],
    ipv6_local_destination_hooks: [bool; NftIpv4Hook::COUNT],
    /// Hook order is compiled once when the candidate becomes immutable.
    /// Packet evaluation never sorts or takes the writer lock.
    ipv4_hooks: [HookProgram; NftIpv4Hook::COUNT],
    ipv6_hooks: [HookProgram; NftIpv4Hook::COUNT],
}

impl RulesetSnapshot {
    fn reconcile_nat_hooks(&mut self, pending: [Option<u64>; 2]) -> Result<(), SystemError> {
        let mut needed = [false; 2];
        for table in &self.tables {
            if table.chains.iter().any(|chain| {
                chain
                    .base
                    .is_some_and(|base| base.chain_type == NftChainType::Nat)
            }) {
                needed[0] |= matches!(table.family, 1 | 2);
                needed[1] |= matches!(table.family, 1 | 10);
            }
        }
        for index in 0..2 {
            if !needed[index] {
                self.nat_outer_hook_order[index] = None;
            } else if self.nat_outer_hook_order[index].is_none() {
                self.nat_outer_hook_order[index] = Some(pending[index].ok_or(SystemError::EINVAL)?);
            }
        }
        Ok(())
    }
    /// CT expressions and NAT base chains hold tracking for their table
    /// family. An inet table needs both protocols.
    fn conntrack_consumer_families(&self) -> [bool; 2] {
        let mut needed = [false; 2];
        for table in &self.tables {
            if table.chains.iter().any(|chain| {
                chain
                    .base
                    .is_some_and(|base| base.chain_type == NftChainType::Nat)
                    || chain.rules.iter().any(|rule| rule.requires_conntrack())
            }) {
                needed[0] |= matches!(table.family, 1 | 2);
                needed[1] |= matches!(table.family, 1 | 10);
            }
        }
        needed
    }

    /// Retire CT hooks from the new snapshot when its last consumer goes
    /// away. Older readers keep their pinned snapshot and the namespace's
    /// preallocated runtime, so removing a rule cannot invalidate them.
    fn reconcile_conntrack_hooks(
        &mut self,
        pending_order: [Option<u64>; 2],
    ) -> Result<[bool; 2], SystemError> {
        let needed = self.conntrack_consumer_families();
        let mut newly_registered = [false; 2];
        for (index, needed) in needed.into_iter().enumerate() {
            if !needed {
                self.conntrack_hook_order[index] = None;
            } else if self.conntrack_hook_order[index].is_none() {
                self.conntrack_hook_order[index] =
                    Some(pending_order[index].ok_or(SystemError::EINVAL)?);
                newly_registered[index] = true;
            }
        }
        Ok(newly_registered)
    }

    pub(crate) fn conntrack_registered(&self, version: IpVersion) -> bool {
        self.conntrack_hook_order[usize::from(version == IpVersion::Ipv6)].is_some()
    }

    pub(crate) fn requires_route_lookup(&self) -> bool {
        self.requires_route_lookup
    }

    pub(crate) fn requires_iface_names(&self) -> bool {
        self.requires_iface_names
    }

    pub(crate) fn hook_requires_iface_names(&self, hook: NftIpv4Hook) -> bool {
        self.iface_name_hooks[hook.index()]
    }

    pub(crate) fn ipv4_hook_requires_route_lookup(&self, hook: NftIpv4Hook) -> bool {
        self.ipv4_route_lookup_hooks[hook.index()]
    }

    pub(crate) fn has_ipv4_hook(&self, hook: NftIpv4Hook) -> bool {
        !self.ipv4_hooks[hook.index()].is_empty()
            || (matches!(hook, NftIpv4Hook::PreRouting | NftIpv4Hook::LocalOut)
                && self.conntrack_registered(IpVersion::Ipv4))
    }

    pub(crate) fn has_ipv6_hook(&self, hook: NftIpv4Hook) -> bool {
        !self.ipv6_hooks[hook.index()].is_empty()
            || (matches!(hook, NftIpv4Hook::PreRouting | NftIpv4Hook::LocalOut)
                && self.conntrack_registered(IpVersion::Ipv6))
    }

    /// A protected FIB view is needed only when an IPv6 hook can execute an
    /// xt addrtype predicate, including through a jump to a regular chain.
    pub(crate) fn ipv6_hook_requires_local_destination(&self, hook: NftIpv4Hook) -> bool {
        self.ipv6_local_destination_hooks[hook.index()]
    }

    pub(crate) fn has_output_hook(&self) -> bool {
        [NftIpv4Hook::LocalOut, NftIpv4Hook::PostRouting]
            .into_iter()
            .any(|hook| self.has_ipv4_hook(hook) || self.has_ipv6_hook(hook))
    }

    /// NAT outer hooks exist while a NAT base chain remains registered.
    /// Callers use this before taking a mutable/COW packet buffer; ordinary
    /// filter and CT-only paths keep their read-only fast path.
    pub(crate) fn hook_may_nat(&self, version: IpVersion, hook: NftIpv4Hook) -> bool {
        hook != NftIpv4Hook::Forward
            && self.nat_outer_hook_order[usize::from(version == IpVersion::Ipv6)].is_some()
    }

    /// Used by output preparation to sample interface addresses only when
    /// MASQUERADE might execute. Conservative across regular-chain jumps.
    pub(crate) fn requires_masquerade(&self, version: IpVersion, hook: NftIpv4Hook) -> bool {
        if hook != NftIpv4Hook::PostRouting || !self.hook_may_nat(version, hook) {
            return false;
        }
        let family = if version == IpVersion::Ipv4 { 2 } else { 10 };
        self.tables
            .iter()
            .filter(|table| table.family == 1 || table.family == family)
            .any(|table| {
                table.chains.iter().any(|chain| {
                    chain.rules.iter().any(|rule| {
                        rule.expressions.iter().any(|expression| {
                            matches!(expression, NftExpression::Masquerade { .. })
                        })
                    })
                })
            })
    }

    /// Check only NAT chains reachable at this hook; fixed NAT outer hooks
    /// alone do not require an ingress-address snapshot.
    pub(crate) fn requires_redirect(&self, version: IpVersion, hook: NftIpv4Hook) -> bool {
        if !matches!(hook, NftIpv4Hook::PreRouting | NftIpv4Hook::LocalOut) {
            return false;
        }
        let program = if version == IpVersion::Ipv4 {
            &self.ipv4_hooks[hook.index()]
        } else {
            &self.ipv6_hooks[hook.index()]
        };
        program.steps.iter().any(|step| match step {
            HookStep::NatOuter { chains, .. } => chains
                .iter()
                .any(|(table, chain)| table.chain_requires_redirect(chain, 0)),
            _ => false,
        })
    }

    pub(crate) fn ipv6_hook_requires_iface_names(&self, hook: NftIpv4Hook) -> bool {
        self.ipv6_iface_name_hooks[hook.index()]
    }

    pub(crate) fn allows_ipv6_hook(&self, hook: NftIpv4Hook, packet: &NftPacket<'_>) -> bool {
        self.ipv6_hooks[hook.index()].evaluate(packet) == NftVerdict::Accept
    }

    pub(crate) fn allows_ipv6_hook_with_tracking(
        &self,
        hook: NftIpv4Hook,
        packet: &NftPacket<'_>,
        track: impl FnMut() -> bool,
    ) -> bool {
        self.ipv6_hooks[hook.index()].evaluate_with_tracking(packet, track) == NftVerdict::Accept
    }

    pub(crate) fn allows_ipv4_hook(&self, hook: NftIpv4Hook, packet: &NftPacket<'_>) -> bool {
        self.ipv4_hooks[hook.index()].evaluate(packet) == NftVerdict::Accept
    }

    pub(crate) fn allows_ipv4_hook_with_tracking(
        &self,
        hook: NftIpv4Hook,
        packet: &NftPacket<'_>,
        track: impl FnMut() -> bool,
    ) -> bool {
        self.ipv4_hooks[hook.index()].evaluate_with_tracking(packet, track) == NftVerdict::Accept
    }

    pub(crate) fn evaluate_ipv4_hook_with_nat(
        &self,
        hook: NftIpv4Hook,
        packet: NftNatPacket<'_>,
        track: impl FnMut(&[u8]) -> bool,
        nat: impl FnMut(&mut [u8], NftNatEvent) -> Result<NftNatProgress, SystemError>,
    ) -> Result<bool, SystemError> {
        self.ipv4_hooks[hook.index()].evaluate_with_nat(packet, track, nat)
    }

    pub(crate) fn evaluate_ipv6_hook_with_nat(
        &self,
        hook: NftIpv4Hook,
        packet: NftNatPacket<'_>,
        track: impl FnMut(&[u8]) -> bool,
        nat: impl FnMut(&mut [u8], NftNatEvent) -> Result<NftNatProgress, SystemError>,
    ) -> Result<bool, SystemError> {
        self.ipv6_hooks[hook.index()].evaluate_with_nat(packet, track, nat)
    }

    fn compile_ipv4_hooks(&mut self) -> Result<(), SystemError> {
        for hook in [
            NftIpv4Hook::PreRouting,
            NftIpv4Hook::LocalIn,
            NftIpv4Hook::Forward,
            NftIpv4Hook::LocalOut,
            NftIpv4Hook::PostRouting,
        ] {
            self.ipv4_hooks[hook.index()] = self.compile_hook(hook, &[1, 2])?;
            self.iface_name_hooks[hook.index()] =
                self.ipv4_hooks[hook.index()].requires_iface_names();
            self.ipv4_route_lookup_hooks[hook.index()] =
                self.ipv4_hooks[hook.index()].requires_route_lookup();
        }
        for hook in [
            NftIpv4Hook::PreRouting,
            NftIpv4Hook::LocalIn,
            NftIpv4Hook::Forward,
            NftIpv4Hook::LocalOut,
            NftIpv4Hook::PostRouting,
        ] {
            self.ipv6_hooks[hook.index()] = self.compile_hook(hook, &[1, 10])?;
            self.ipv6_iface_name_hooks[hook.index()] =
                self.ipv6_hooks[hook.index()].requires_iface_names();
            self.ipv6_local_destination_hooks[hook.index()] =
                self.ipv6_hooks[hook.index()].requires_route_lookup();
        }
        self.requires_iface_names = self.iface_name_hooks.iter().any(|needed| *needed)
            || self.ipv6_iface_name_hooks.iter().any(|needed| *needed);
        Ok(())
    }

    fn compile_hook(&self, hook: NftIpv4Hook, families: &[u8]) -> Result<HookProgram, SystemError> {
        let conntrack_order = self.conntrack_hook_order[usize::from(families.contains(&10))]
            .filter(|_| matches!(hook, NftIpv4Hook::PreRouting | NftIpv4Hook::LocalOut));
        let nat_side = match hook {
            NftIpv4Hook::PreRouting | NftIpv4Hook::LocalOut => Some(NatManipSide::Destination),
            NftIpv4Hook::LocalIn | NftIpv4Hook::PostRouting => Some(NatManipSide::Source),
            NftIpv4Hook::Forward => None,
        };
        let chain_count = self
            .tables
            .iter()
            .filter(|table| families.contains(&table.family))
            .try_fold(0usize, |total, table| {
                total.checked_add(
                    table
                        .chains
                        .iter()
                        .filter(|chain| chain.base.is_some_and(|base| base.hook == hook))
                        .count(),
                )
            })
            .ok_or(SystemError::ENOSPC)?;
        let mut steps = Vec::new();
        let mut nat_chains = Vec::new();
        steps
            .try_reserve_exact(
                chain_count
                    .checked_add(usize::from(conntrack_order.is_some()))
                    .ok_or(SystemError::ENOSPC)?,
            )
            .map_err(|_| SystemError::ENOMEM)?;
        nat_chains
            .try_reserve_exact(chain_count)
            .map_err(|_| SystemError::ENOMEM)?;
        if let Some(registration) = conntrack_order {
            steps.push(HookStep::Conntrack { registration });
        }
        for table in self
            .tables
            .iter()
            .filter(|table| families.contains(&table.family))
        {
            for chain in table
                .chains
                .iter()
                .filter(|chain| chain.base.is_some_and(|base| base.hook == hook))
            {
                if chain.base.unwrap().chain_type == NftChainType::Nat {
                    nat_chains.push((table.clone(), chain.clone()));
                } else {
                    steps.push(HookStep::FilterChain {
                        table: table.clone(),
                        chain: chain.clone(),
                    });
                }
            }
        }
        if nat_side.is_some()
            && self.nat_outer_hook_order[usize::from(families.contains(&10))].is_some()
        {
            let side = nat_side.ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
            nat_chains.sort_unstable_by(|left, right| {
                let left = left.1.base.unwrap();
                let right = right.1.base.unwrap();
                left.priority
                    .cmp(&right.priority)
                    .then_with(|| right.hook_order.cmp(&left.hook_order))
            });
            let registration = self.nat_outer_hook_order[usize::from(families.contains(&10))]
                .ok_or(SystemError::EINVAL)?;
            steps.push(HookStep::NatOuter {
                side,
                registration,
                chains: nat_chains,
            });
        }
        // Linux's nf_hook_entries_grow inserts a newly registered hook
        // before older hooks at the same priority. Keep that ordering without
        // requiring an allocating stable sort in the commit path.
        steps.sort_unstable_by(|left, right| {
            let order = |step: &HookStep| match step {
                HookStep::Conntrack { registration } => (-200, *registration),
                HookStep::NatOuter {
                    side, registration, ..
                } => (
                    if *side == NatManipSide::Destination {
                        -100
                    } else {
                        100
                    },
                    *registration,
                ),
                HookStep::FilterChain { chain, .. } => {
                    let base = chain.base.unwrap();
                    (base.priority, base.hook_order)
                }
            };
            let (left_priority, left_registration) = order(left);
            let (right_priority, right_registration) = order(right);
            left_priority
                .cmp(&right_priority)
                .then_with(|| right_registration.cmp(&left_registration))
        });
        Ok(HookProgram { steps })
    }
}

#[derive(Debug)]
pub(crate) struct NftNamespaceState {
    ruleset: RcuArcSlot<RulesetSnapshot>,
    writer: Mutex<()>,
}

/// A batch owns one candidate snapshot. Dropping it before `commit` aborts
/// every table mutation without changing the published ruleset.
pub(crate) struct NftTransaction<'a> {
    state: &'a NftNamespaceState,
    writer: MutexGuard<'a, ()>,
    candidate: RulesetSnapshot,
    /// Reserve Linux's hook registration order at successful NEWRULE time;
    /// actual runtime activation waits until commit of the final candidate.
    pending_ct_hook_order: [Option<u64>; 2],
    pending_nat_outer_hook_order: [Option<u64>; 2],
    pending_set_ids: Vec<(u8, Vec<u8>, u32, u64)>,
    changed: bool,
}

pub(crate) struct NewSetSpec<'a> {
    pub(crate) family: u8,
    pub(crate) table_name: &'a [u8],
    pub(crate) name: &'a [u8],
    pub(crate) key_type: u32,
    pub(crate) key_len: usize,
    pub(crate) data_type: Option<u32>,
    pub(crate) data_len: Option<usize>,
    pub(crate) size: Option<usize>,
    pub(crate) userdata: &'a [u8],
    pub(crate) flags: u32,
    pub(crate) id: Option<u32>,
    pub(crate) exclusive: bool,
}

type CreatedSet = (Arc<NftTable>, Arc<NftSet>);

impl NftNamespaceState {
    pub(crate) fn new() -> Self {
        Self {
            ruleset: RcuArcSlot::new(Arc::new(RulesetSnapshot {
                generation: 1,
                tables: Vec::new(),
                next_table_handle: 1,
                next_hook_order: 1,
                conntrack_hook_order: [None; 2],
                nat_outer_hook_order: [None; 2],
                requires_route_lookup: false,
                requires_iface_names: false,
                iface_name_hooks: [false; NftIpv4Hook::COUNT],
                ipv4_route_lookup_hooks: [false; NftIpv4Hook::COUNT],
                ipv6_iface_name_hooks: [false; NftIpv4Hook::COUNT],
                ipv6_local_destination_hooks: [false; NftIpv4Hook::COUNT],
                ipv4_hooks: core::array::from_fn(|_| HookProgram::empty()),
                ipv6_hooks: core::array::from_fn(|_| HookProgram::empty()),
            })),
            writer: Mutex::new(()),
        }
    }

    pub(crate) fn generation(&self) -> u32 {
        self.ruleset.with_read(|ruleset| ruleset.generation)
    }

    pub(crate) fn snapshot(&self) -> Arc<RulesetSnapshot> {
        self.ruleset.load()
    }

    pub(crate) fn transaction(
        &self,
        expected_generation: u32,
    ) -> Result<NftTransaction<'_>, SystemError> {
        let writer = self.writer.lock();
        let current = self.ruleset.load();
        if expected_generation != 0 && expected_generation != current.generation {
            return Err(SystemError::ERESTART);
        }
        let mut tables = Vec::new();
        tables
            .try_reserve_exact(current.tables.len())
            .map_err(|_| SystemError::ENOMEM)?;
        tables.extend(current.tables.iter().cloned());
        Ok(NftTransaction {
            state: self,
            writer,
            candidate: RulesetSnapshot {
                generation: current.generation,
                tables,
                next_table_handle: current.next_table_handle,
                next_hook_order: current.next_hook_order,
                conntrack_hook_order: current.conntrack_hook_order,
                nat_outer_hook_order: current.nat_outer_hook_order,
                requires_route_lookup: false,
                requires_iface_names: false,
                iface_name_hooks: [false; NftIpv4Hook::COUNT],
                ipv4_route_lookup_hooks: [false; NftIpv4Hook::COUNT],
                ipv6_iface_name_hooks: [false; NftIpv4Hook::COUNT],
                ipv6_local_destination_hooks: [false; NftIpv4Hook::COUNT],
                ipv4_hooks: core::array::from_fn(|_| HookProgram::empty()),
                ipv6_hooks: core::array::from_fn(|_| HookProgram::empty()),
            },
            pending_ct_hook_order: [None; 2],
            pending_nat_outer_hook_order: [None; 2],
            pending_set_ids: Vec::new(),
            changed: false,
        })
    }
}

type UpdatedSetElement = Option<(Arc<NftTable>, Arc<NftSet>)>;

impl NftTransaction<'_> {
    fn table_for_write(&mut self, index: usize) -> Result<&mut NftTable, SystemError> {
        if Arc::get_mut(&mut self.candidate.tables[index]).is_none() {
            self.candidate.tables[index] = self.candidate.tables[index].clone_for_write()?;
        }
        Ok(Arc::get_mut(&mut self.candidate.tables[index]).unwrap())
    }

    /// Exact single-element updates can reuse the transaction-owned set
    /// after its first COW. All fallible validation/allocation precedes the
    /// mutation, preserving the per-message rollback contract.
    fn try_update_single_exact_element(
        &mut self,
        table_index: usize,
        set_index: usize,
        element: &NftSetElementInput<'_>,
        add: bool,
    ) -> Result<UpdatedSetElement, SystemError> {
        let table = self.table_for_write(table_index)?;
        if Arc::get_mut(&mut table.sets[set_index]).is_none()
            || table.sets[set_index].flags & 4 != 0
        {
            return Ok(None);
        }
        let verdict = match element.verdict {
            Some(NftRuleInput::Accept) => Some(NftRuleVerdict::Accept),
            Some(NftRuleInput::Drop) => Some(NftRuleVerdict::Drop),
            Some(NftRuleInput::Continue) => Some(NftRuleVerdict::Continue),
            Some(NftRuleInput::Return) => Some(NftRuleVerdict::Return),
            Some(NftRuleInput::Jump(name) | NftRuleInput::Goto(name)) => {
                let target = table
                    .chains
                    .iter()
                    .find(|chain| chain.name == name)
                    .ok_or(SystemError::ENOENT)?;
                if target.base.is_some() {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                Some(if matches!(element.verdict, Some(NftRuleInput::Jump(_))) {
                    NftRuleVerdict::Jump(target.handle)
                } else {
                    NftRuleVerdict::Goto(target.handle)
                })
            }
            None => None,
        };
        let set = Arc::get_mut(&mut table.sets[set_index]).unwrap();
        let verdict_map = set.data_type == Some(0xffff_ff00);
        if element.key.len() != set.key_len || element.flags != 0 || element.key_end.is_some() {
            return Err(SystemError::EINVAL);
        }
        if add
            && (if verdict_map {
                element.value.is_some() || element.verdict.is_none()
            } else {
                element.verdict.is_some() || element.value.map(|value| value.len()) != set.data_len
            })
        {
            return Err(SystemError::EINVAL);
        }
        let position = set
            .elements
            .binary_search_by(|candidate| candidate.key.as_slice().cmp(element.key));
        match (add, position) {
            (true, Ok(_)) => return Err(SystemError::EEXIST),
            (false, Err(_)) => return Err(SystemError::ENOENT),
            (false, Ok(index)) => {
                set.elements.remove(index);
            }
            (true, Err(index)) => {
                if set
                    .size
                    .is_some_and(|size| size != 0 && set.elements.len() >= size)
                {
                    return Err(SystemError::ENFILE);
                }
                let inserted = NftSetElement {
                    key: copy_bytes(element.key)?,
                    value: element.value.map(copy_bytes).transpose()?,
                    verdict,
                    flags: 0,
                };
                set.elements
                    .try_reserve(1)
                    .map_err(|_| SystemError::ENOMEM)?;
                set.elements.insert(index, inserted);
            }
        }
        let result = (
            self.candidate.tables[table_index].clone(),
            self.candidate.tables[table_index].sets[set_index].clone(),
        );
        self.changed = true;
        Ok(Some(result))
    }

    pub(crate) fn new_set(
        &mut self,
        NewSetSpec {
            family,
            table_name,
            name,
            key_type,
            key_len,
            data_type,
            data_len,
            size,
            userdata,
            flags,
            id,
            exclusive,
        }: NewSetSpec<'_>,
    ) -> Result<Option<CreatedSet>, SystemError> {
        if flags & !(1 | 2 | 4 | 8) != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if flags & 1 != 0 && id.is_none() {
            // Anonymous set binding is transaction-scoped; a bare template
            // name cannot identify its generated instance safely.
            return Err(SystemError::EINVAL);
        }
        if (flags & 8 != 0) != (data_type.is_some() && data_len.is_some()) {
            return Err(SystemError::EINVAL);
        }
        let verdict_map = data_type == Some(0xffff_ff00);
        if verdict_map && data_len != Some(0) {
            return Err(SystemError::EINVAL);
        }
        if name.is_empty()
            || name.len() >= 256
            || name.contains(&0)
            || key_len == 0
            || key_len > 64
            || data_len.is_some_and(|len| len > 64 || len == 0 && !verdict_map)
            || userdata.len() > 256
        {
            return Err(SystemError::EINVAL);
        }
        let index = self
            .candidate
            .tables
            .iter()
            .position(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let original = &self.candidate.tables[index];
        let allocated_name = allocated_set_name(original, name)?;
        if original.sets.iter().any(|set| set.name == allocated_name) {
            return if exclusive {
                Err(SystemError::EEXIST)
            } else {
                Ok(None)
            };
        }
        let handle = original.next_handle;
        let next_handle = handle.checked_add(1).ok_or(SystemError::ENOSPC)?;
        let pending_id = if let Some(id) = id {
            if self
                .pending_set_ids
                .iter()
                .any(|entry| entry.0 == family && entry.1 == table_name && entry.2 == id)
            {
                return Err(SystemError::EEXIST);
            }
            self.pending_set_ids
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            Some((family, copy_bytes(table_name)?, id, handle))
        } else {
            None
        };
        let set = Arc::try_new(NftSet {
            name: allocated_name,
            handle,
            key_type,
            key_len,
            flags,
            data_type,
            data_len,
            size,
            userdata: copy_bytes(userdata)?,
            elements: Vec::new(),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let table = self.table_for_write(index)?;
        debug_assert!(table.sets.last().is_none_or(|set| set.handle < handle));
        table.sets.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        table.sets.push(set.clone());
        table.next_handle = next_handle;
        table.use_count = table.use_count.checked_add(1).ok_or(SystemError::EMFILE)?;
        if let Some(entry) = pending_id {
            self.pending_set_ids.push(entry);
        }
        self.changed = true;
        Ok(Some((self.candidate.tables[index].clone(), set)))
    }

    pub(crate) fn del_set(
        &mut self,
        family: u8,
        table_name: &[u8],
        name: &[u8],
    ) -> Result<(Arc<NftTable>, Arc<NftSet>), SystemError> {
        let index = self
            .candidate
            .tables
            .iter()
            .position(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let table = &self.candidate.tables[index];
        let set_index = table
            .sets
            .iter()
            .position(|set| set.name == name)
            .ok_or(SystemError::ENOENT)?;
        let handle = table.sets[set_index].handle;
        if table
            .chains
            .iter()
            .flat_map(|chain| chain.rules.iter())
            .any(|rule| {
                rule.expressions.iter().any(|expr| {
                    matches!(expr,
                NftExpression::Lookup { set_handle, .. } if *set_handle == handle)
                })
            })
        {
            return Err(SystemError::EBUSY);
        }
        let table = self.table_for_write(index)?;
        let set = table.sets.remove(set_index);
        table.use_count -= 1;
        self.pending_set_ids
            .retain(|entry| !(entry.0 == family && entry.1 == table_name && entry.3 == handle));
        self.changed = true;
        Ok((self.candidate.tables[index].clone(), set))
    }

    pub(crate) fn update_set_elements(
        &mut self,
        family: u8,
        table_name: &[u8],
        name: &[u8],
        elements: &[NftSetElementInput<'_>],
        add: bool,
    ) -> Result<(Arc<NftTable>, Arc<NftSet>), SystemError> {
        let index = self
            .candidate
            .tables
            .iter()
            .position(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let set_index = self.candidate.tables[index]
            .sets
            .iter()
            .position(|set| set.name == name)
            .ok_or(SystemError::ENOENT)?;
        let target_handle = self.candidate.tables[index].sets[set_index].handle;
        if self.candidate.tables[index].sets[set_index].flags & 2 != 0
            && self.candidate.tables[index]
                .chains
                .iter()
                .flat_map(|chain| chain.rules.iter())
                .any(|rule| {
                    rule.expressions.iter().any(|expression| {
                        matches!(expression, NftExpression::Lookup { set_handle, .. }
                            if *set_handle == target_handle)
                    })
                })
        {
            return Err(SystemError::EBUSY);
        }
        if let [element] = elements {
            if let Some(result) =
                self.try_update_single_exact_element(index, set_index, element, add)?
            {
                return Ok(result);
            }
        }
        if elements.is_empty() {
            return Ok((
                self.candidate.tables[index].clone(),
                self.candidate.tables[index].sets[set_index].clone(),
            ));
        }
        // A batch's first write still COWs the published set. Subsequent
        // messages can edit its private copy, provided every failed message
        // rolls back before returning to the transaction caller.
        enum Undo {
            Inserted(usize),
            Removed(usize, NftSetElement),
            RemovedRange(usize, NftSetElement, NftSetElement),
        }
        let updated = {
            let table = self.table_for_write(index)?;
            let mut replacement = if Arc::get_mut(&mut table.sets[set_index]).is_none() {
                Some(table.sets[set_index].clone_for_write()?)
            } else {
                None
            };
            let set = if let Some(replacement) = replacement.as_mut() {
                Arc::get_mut(replacement).unwrap()
            } else {
                Arc::get_mut(&mut table.sets[set_index]).unwrap()
            };
            let mut undo = Vec::new();
            undo.try_reserve_exact(elements.len())
                .map_err(|_| SystemError::ENOMEM)?;
            if add {
                set.elements
                    .try_reserve(elements.len())
                    .map_err(|_| SystemError::ENOMEM)?;
            }
            let result = (|| -> Result<(), SystemError> {
                for element in elements {
                    let verdict_map = set.data_type == Some(0xffff_ff00);
                    let interval = set.flags & 4 != 0;
                    let end_boundary = element.flags & 1 != 0;
                    if element.key.len() != set.key_len
                        || element.flags & !1 != 0
                        || (end_boundary && !interval)
                        || element.key_end.is_some_and(|key| key.len() != set.key_len)
                        || (add && element.key_end.is_some())
                        || (!interval && element.key_end.is_some())
                    {
                        return Err(SystemError::EINVAL);
                    }
                    if add
                        && (if end_boundary {
                            element.value.is_some() || element.verdict.is_some()
                        } else if verdict_map {
                            element.value.is_some() || element.verdict.is_none()
                        } else {
                            element.verdict.is_some()
                                || element.value.map(|value| value.len()) != set.data_len
                        })
                    {
                        return Err(SystemError::EINVAL);
                    }
                    if let (false, Some(end)) = (add, element.key_end) {
                        // Ubuntu's nft emits KEY+KEY_END for CIDR deletion. Linux
                        // 6.6 stores boundary elements; require this exact pair so
                        // a malformed range cannot remove adjacent intervals.
                        let start = set
                            .element_index(element.key, 0)
                            .ok_or(SystemError::ENOENT)?;
                        let last = set.element_index(end, 1).ok_or(SystemError::ENOENT)?;
                        if last != start + 1
                            || set.elements[start].flags & 1 != 0
                            || set.elements[last].flags & 1 == 0
                        {
                            return Err(SystemError::EINVAL);
                        }
                        let last_element = set.elements.remove(last);
                        let first_element = set.elements.remove(start);
                        undo.push(Undo::RemovedRange(start, first_element, last_element));
                        continue;
                    }
                    let verdict = match element.verdict {
                        Some(NftRuleInput::Accept) => Some(NftRuleVerdict::Accept),
                        Some(NftRuleInput::Drop) => Some(NftRuleVerdict::Drop),
                        Some(NftRuleInput::Continue) => Some(NftRuleVerdict::Continue),
                        Some(NftRuleInput::Return) => Some(NftRuleVerdict::Return),
                        Some(NftRuleInput::Jump(name) | NftRuleInput::Goto(name)) => {
                            let target = table
                                .chains
                                .iter()
                                .find(|chain| chain.name == name)
                                .ok_or(SystemError::ENOENT)?;
                            if target.base.is_some() {
                                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                            }
                            Some(if matches!(element.verdict, Some(NftRuleInput::Jump(_))) {
                                NftRuleVerdict::Jump(target.handle)
                            } else {
                                NftRuleVerdict::Goto(target.handle)
                            })
                        }
                        None => None,
                    };
                    match set.element_position(element.key, element.flags) {
                        Ok(_) if add => return Err(SystemError::EEXIST),
                        Ok(position) => {
                            undo.push(Undo::Removed(position, set.elements.remove(position)));
                        }
                        Err(_) if !add => return Err(SystemError::ENOENT),
                        Err(position) => {
                            if let Some(size) = set.size.filter(|size| *size != 0) {
                                let limit = if interval {
                                    // Linux rbtree allocates two boundary nodes per
                                    // logical interval plus the optional all-zero
                                    // no-match sentinel.
                                    let sentinel = set.elements.first().is_some_and(|first| {
                                        first.flags & 1 != 0
                                            && first.key.iter().all(|byte| *byte == 0)
                                    }) || end_boundary
                                        && element.key.iter().all(|byte| *byte == 0);
                                    size.saturating_mul(2).saturating_add(usize::from(sentinel))
                                } else {
                                    size
                                };
                                if set.elements.len() >= limit {
                                    return Err(SystemError::ENFILE);
                                }
                            }
                            let inserted = NftSetElement {
                                key: copy_bytes(element.key)?,
                                value: element.value.map(copy_bytes).transpose()?,
                                verdict,
                                flags: element.flags,
                            };
                            set.elements.insert(position, inserted);
                            undo.push(Undo::Inserted(position));
                        }
                    }
                }
                if set.flags & 4 != 0 {
                    let mut active = false;
                    for (position, element) in set.elements.iter().enumerate() {
                        let end = element.flags & 1 != 0;
                        if end && !active && position != 0 || !end && active {
                            return Err(SystemError::EINVAL);
                        }
                        active = !end;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                for action in undo.into_iter().rev() {
                    match action {
                        Undo::Inserted(position) => {
                            set.elements.remove(position);
                        }
                        Undo::Removed(position, element) => set.elements.insert(position, element),
                        Undo::RemovedRange(position, first, last) => {
                            set.elements.insert(position, first);
                            set.elements.insert(position + 1, last);
                        }
                    }
                }
                return Err(error);
            }
            if let Some(replacement) = replacement {
                table.sets[set_index] = replacement;
            }
            table.sets[set_index].clone()
        };
        self.changed = true;
        Ok((self.candidate.tables[index].clone(), updated))
    }

    pub(crate) fn set_for_notification(
        &self,
        family: u8,
        table_name: &[u8],
        name: &[u8],
    ) -> Result<(Arc<NftTable>, Arc<NftSet>), SystemError> {
        let table = self
            .candidate
            .tables
            .iter()
            .find(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let set = table
            .sets
            .iter()
            .find(|set| set.name == name)
            .ok_or(SystemError::ENOENT)?;
        Ok((table.clone(), set.clone()))
    }

    pub(crate) fn resolve_set_name(
        &self,
        family: u8,
        table_name: &[u8],
        name: Option<&[u8]>,
        id: Option<u32>,
    ) -> Result<Vec<u8>, SystemError> {
        let table = self
            .candidate
            .tables
            .iter()
            .find(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let by_name = name.and_then(|name| table.sets.iter().find(|set| set.name == name));
        let by_id = id.and_then(|id| {
            self.pending_set_ids
                .iter()
                .find(|entry| entry.0 == family && entry.1 == table_name && entry.2 == id)
                .and_then(|entry| table.sets.iter().find(|set| set.handle == entry.3))
        });
        if by_name.is_some() && by_id.is_some() && by_name.unwrap().handle != by_id.unwrap().handle
        {
            return Err(SystemError::EINVAL);
        }
        copy_bytes(&by_name.or(by_id).ok_or(SystemError::ENOENT)?.name)
    }

    pub(crate) fn new_table(
        &mut self,
        family: u8,
        name: &[u8],
        flags: u32,
        userdata: &[u8],
        exclusive: bool,
        replace: bool,
    ) -> Result<Option<Arc<NftTable>>, SystemError> {
        if !matches!(family, 1 | 2 | 10) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if name.is_empty() || name.len() >= 256 || name.contains(&0) {
            return Err(SystemError::EINVAL);
        }
        if userdata.len() > 256 {
            return Err(SystemError::ERANGE);
        }
        // OWNER needs socket-close cleanup and DORMANT needs hook lifecycle.
        // Neither may be acknowledged until those semantics are implemented.
        if flags != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if self
            .candidate
            .tables
            .iter()
            .any(|table| table.family == family && table.name == name)
        {
            return if exclusive {
                Err(SystemError::EEXIST)
            } else if replace {
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            } else {
                Ok(None)
            };
        }
        let handle = self.candidate.next_table_handle;
        let next_handle = handle.checked_add(1).ok_or(SystemError::ENOSPC)?;
        let table = Arc::try_new(NftTable {
            family,
            name: copy_bytes(name)?,
            handle,
            flags,
            userdata: copy_bytes(userdata)?,
            use_count: 0,
            chains: Vec::new(),
            sets: Vec::new(),
            next_handle: 1,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        self.candidate
            .tables
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        self.candidate.tables.push(table.clone());
        self.candidate.next_table_handle = next_handle;
        self.changed = true;
        Ok(Some(table))
    }

    /// Create a regular chain or an executable IP base chain.
    /// Unsupported hook configurations are rejected before publication.
    pub(crate) fn new_ip_chain(
        &mut self,
        family: u8,
        table_name: &[u8],
        name: &[u8],
        hook: Option<(NftIpv4Hook, i32, NftVerdict, NftChainType)>,
        exclusive: bool,
        replace: bool,
    ) -> Result<Option<CreatedChain>, SystemError> {
        if hook.is_some_and(|(selected, priority, _, _)| {
            selected == NftIpv4Hook::PreRouting && priority <= -400
        }) {
            // Defragmentation is not yet installed as a separate -400 hook.
            // An earlier chain would run at the wrong packet boundary once
            // conntrack or another defrag consumer is enabled.
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if hook.is_some_and(|(selected, priority, policy, chain_type)| {
            chain_type == NftChainType::Nat
                && (policy != NftVerdict::Accept
                    || priority <= -200
                    || selected == NftIpv4Hook::Forward)
        }) {
            // Linux requires NAT chains to follow conntrack. Their user
            // priorities order chains *inside* the fixed outer NAT hook;
            // they do not move the outer rewrite from -100/+100.
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if name.is_empty() || name.len() >= 256 || name.contains(&0) {
            return Err(SystemError::EINVAL);
        }
        let table_index = self
            .candidate
            .tables
            .iter()
            .position(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let table = &self.candidate.tables[table_index];
        if table.chains.iter().any(|chain| chain.name == name) {
            return if exclusive {
                Err(SystemError::EEXIST)
            } else if replace {
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            } else {
                Ok(None)
            };
        }
        let handle = table.next_handle;
        let next_handle = handle.checked_add(1).ok_or(SystemError::ENOSPC)?;
        let hook_order = self.candidate.next_hook_order;
        let mut next_hook_order = if hook.is_some() {
            hook_order.checked_add(1).ok_or(SystemError::ENOSPC)?
        } else {
            hook_order
        };
        let mut pending_ct_hook_order = self.pending_ct_hook_order;
        let mut pending_nat_outer_hook_order = self.pending_nat_outer_hook_order;
        if hook.is_some_and(|(_, _, _, chain_type)| chain_type == NftChainType::Nat) {
            for (index, applies) in [matches!(family, 1 | 2), matches!(family, 1 | 10)]
                .into_iter()
                .enumerate()
            {
                if applies
                    && self.candidate.nat_outer_hook_order[index].is_none()
                    && pending_nat_outer_hook_order[index].is_none()
                {
                    pending_nat_outer_hook_order[index] = Some(hook_order);
                }
                if applies
                    && self.candidate.conntrack_hook_order[index].is_none()
                    && pending_ct_hook_order[index].is_none()
                {
                    pending_ct_hook_order[index] = Some(next_hook_order);
                    next_hook_order = next_hook_order.checked_add(1).ok_or(SystemError::ENOSPC)?;
                }
            }
        }
        let chain = Arc::try_new(NftChain {
            name: copy_bytes(name)?,
            handle,
            base: hook.map(|(hook, priority, policy, chain_type)| NftBaseChain {
                hook_order,
                hook,
                priority,
                policy,
                chain_type,
            }),
            rules: Vec::new(),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let table = self.table_for_write(table_index)?;
        let use_count = table.use_count.checked_add(1).ok_or(SystemError::EMFILE)?;
        table
            .chains
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        table.chains.push(chain.clone());
        table.use_count = use_count;
        table.next_handle = next_handle;
        self.candidate.next_hook_order = next_hook_order;
        self.pending_ct_hook_order = pending_ct_hook_order;
        self.pending_nat_outer_hook_order = pending_nat_outer_hook_order;
        self.changed = true;
        Ok(Some((self.candidate.tables[table_index].clone(), chain)))
    }

    pub(crate) fn del_chain(
        &mut self,
        family: u8,
        table_name: &[u8],
        name: Option<&[u8]>,
        handle: Option<u64>,
        non_recursive: bool,
    ) -> Result<(Arc<NftTable>, Arc<NftChain>), SystemError> {
        let table_index = self
            .candidate
            .tables
            .iter()
            .position(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let chain_index = self.candidate.tables[table_index]
            .chains
            .iter()
            .position(|chain| {
                handle.map_or_else(
                    || name.is_some_and(|name| chain.name == name),
                    |handle| chain.handle == handle,
                )
            })
            .ok_or(SystemError::ENOENT)?;
        let table = &self.candidate.tables[table_index];
        let chain = &table.chains[chain_index];
        if table.inbound_references(chain.handle) != 0 || (non_recursive && !chain.rules.is_empty())
        {
            return Err(SystemError::EBUSY);
        }
        let table = self.table_for_write(table_index)?;
        let chain = table.chains.remove(chain_index);
        table.use_count -= 1;
        let chains = &table.chains;
        let mut reclaimed = 0usize;
        table.sets.retain(|set| {
            let removed_binding = chain.rules.iter().any(|rule| {
                rule.expressions.iter().any(|expression| {
                    matches!(expression, NftExpression::Lookup { set_handle, .. }
                        if *set_handle == set.handle)
                })
            });
            let still_bound = chains
                .iter()
                .flat_map(|chain| chain.rules.iter())
                .any(|rule| {
                    rule.expressions.iter().any(|expression| {
                        matches!(expression, NftExpression::Lookup { set_handle, .. }
                        if *set_handle == set.handle)
                    })
                });
            let keep = set.flags & 1 == 0 || !removed_binding || still_bound;
            if !keep {
                reclaimed += 1;
            }
            keep
        });
        table.use_count -= reclaimed as u32;
        self.changed = true;
        Ok((self.candidate.tables[table_index].clone(), chain))
    }

    pub(crate) fn new_ip_rule(
        &mut self,
        family: u8,
        table_name: &[u8],
        chain_name: &[u8],
        inputs: &[NftExpressionInput<'_>],
        append: bool,
        position: Option<u64>,
    ) -> Result<CreatedRule, SystemError> {
        let table_index = self
            .candidate
            .tables
            .iter()
            .position(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let table = &self.candidate.tables[table_index];
        let chain_index = table
            .chains
            .iter()
            .position(|chain| chain.name == chain_name)
            .ok_or(SystemError::ENOENT)?;
        let rule_index = match position {
            Some(position) => table.chains[chain_index]
                .rules
                .iter()
                .position(|rule| rule.handle == position)
                .ok_or(SystemError::ENOENT)?,
            None if append => table.chains[chain_index].rules.len(),
            None => 0,
        };
        let insertion_index = rule_index + usize::from(position.is_some() && append);
        let handle = table.next_handle;
        let next_handle = handle.checked_add(1).ok_or(SystemError::ENOSPC)?;
        let mut expressions = Vec::new();
        expressions
            .try_reserve_exact(inputs.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let mut initialized = 0u16;
        for (index, input) in inputs.iter().enumerate() {
            let expression = match input {
                NftExpressionInput::XtTcp(info) if family == 2 => {
                    NftExpression::XtTcp(NftXtTcp::from_info(info)?)
                }
                NftExpressionInput::XtAddrtype(info) => {
                    if !matches!(family, 2 | 10) {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    NftExpression::XtAddrtype(NftXtAddrtype::from_info(info)?)
                }
                NftExpressionInput::XtConntrack { revision, info } => {
                    NftExpression::XtConntrack(NftXtConntrack::from_info(*revision, info)?)
                }
                NftExpressionInput::XtTcp(_) => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                NftExpressionInput::Ct {
                    key: 0,
                    dreg,
                    direction: None,
                } => {
                    let dreg = data_register(*dreg, 4)?;
                    initialized |= 1 << (dreg / 4);
                    NftExpression::CtState { dreg }
                }
                NftExpressionInput::Ct { key: 0, .. } => return Err(SystemError::EINVAL),
                NftExpressionInput::Ct { .. } => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                NftExpressionInput::Nat {
                    nat_type,
                    family: nat_family,
                    addr_min_reg,
                    addr_max_reg,
                    proto_min_reg,
                    proto_max_reg,
                    flags,
                } => {
                    if index + 1 != inputs.len()
                        || !matches!(*nat_family, 2 | 10)
                        || (family != 1 && *nat_family != family as u32)
                    {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let side = match *nat_type {
                        0 => NatManipSide::Source,
                        1 => NatManipSide::Destination,
                        _ => return Err(SystemError::EINVAL),
                    };
                    let address_len = if *nat_family == 2 { 4 } else { 16 };
                    let addr_min = addr_min_reg
                        .map(|reg| data_register(reg, address_len))
                        .transpose()?;
                    let addr_max = addr_max_reg
                        .map(|reg| data_register(reg, address_len))
                        .transpose()?;
                    if addr_max.is_some_and(|max| Some(max) != addr_min) {
                        // CT stores one translated address, not an interval.
                        // Do not ACK a rule whose non-singleton range would
                        // otherwise fail only when its first packet arrives.
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let port_min = proto_min_reg.map(|reg| data_register(reg, 2)).transpose()?;
                    let port_max = proto_max_reg.map(|reg| data_register(reg, 2)).transpose()?;
                    let derived_flags =
                        u32::from(addr_min.is_some()) | (u32::from(port_min.is_some()) << 1);
                    if *flags & !derived_flags != 0 {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    for (start, len) in [
                        (addr_min, address_len),
                        (addr_max, address_len),
                        (port_min, 2),
                        (port_max, 2),
                    ] {
                        if let Some(start) = start {
                            let first = start / 4;
                            let last = (start + len - 1) / 4;
                            if (first..=last).any(|slot| initialized & (1 << slot) == 0) {
                                return Err(SystemError::ENODATA);
                            }
                        }
                    }
                    NftExpression::Nat {
                        side,
                        family: *nat_family as u8,
                        addr_min,
                        addr_max,
                        port_min,
                        port_max,
                    }
                }
                NftExpressionInput::Masq {
                    flags,
                    proto_min_reg,
                    proto_max_reg,
                } => {
                    if index + 1 != inputs.len() || *flags & !2 != 0 {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let port_min = proto_min_reg.map(|reg| data_register(reg, 2)).transpose()?;
                    let port_max = proto_max_reg.map(|reg| data_register(reg, 2)).transpose()?;
                    if *flags != if port_min.is_some() { 2 } else { 0 } {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    for start in [port_min, port_max].into_iter().flatten() {
                        if initialized & (1 << (start / 4)) == 0 {
                            return Err(SystemError::ENODATA);
                        }
                    }
                    NftExpression::Masquerade { port_min, port_max }
                }
                NftExpressionInput::Redirect {
                    flags,
                    proto_min_reg,
                    proto_max_reg,
                } => {
                    if index + 1 != inputs.len()
                        || proto_min_reg.is_none() && proto_max_reg.is_some()
                    {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let port_min = proto_min_reg.map(|reg| data_register(reg, 2)).transpose()?;
                    let port_max = proto_max_reg.map(|reg| data_register(reg, 2)).transpose()?;
                    let derived_flags = if port_min.is_some() { 2 } else { 0 };
                    if flags.is_some_and(|flags| flags != derived_flags) {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    for start in [port_min, port_max].into_iter().flatten() {
                        if initialized & (1 << (start / 4)) == 0 {
                            return Err(SystemError::ENODATA);
                        }
                    }
                    NftExpression::Redirect {
                        flags: derived_flags,
                        port_min,
                        port_max,
                    }
                }
                NftExpressionInput::XtTarget {
                    name,
                    revision,
                    info,
                } => {
                    if table.name != b"nat" {
                        return Err(SystemError::EINVAL);
                    }
                    if index + 1 != inputs.len() {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let action = parse_xt_nat_target(family, name, *revision, info)?;
                    NftExpression::XtNatTarget {
                        name: if *name == b"MASQUERADE" {
                            b"MASQUERADE"
                        } else if *name == b"SNAT" {
                            b"SNAT"
                        } else {
                            b"DNAT"
                        },
                        revision: *revision,
                        info: copy_bytes(info)?,
                        action,
                    }
                }
                NftExpressionInput::Meta { key, dreg } => {
                    let key = match key {
                        0 => NftMetaKey::Len,
                        1 => NftMetaKey::Protocol,
                        3 => NftMetaKey::Mark,
                        6 => NftMetaKey::Iifname,
                        7 => NftMetaKey::Oifname,
                        15 => NftMetaKey::Nfproto,
                        16 => NftMetaKey::L4proto,
                        _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                    };
                    let len = match key {
                        NftMetaKey::Iifname | NftMetaKey::Oifname => 16,
                        NftMetaKey::Protocol => 2,
                        _ => 4,
                    };
                    let dreg = data_register(*dreg, len)?;
                    for slot in dreg / 4..=(dreg + len - 1) / 4 {
                        initialized |= 1 << slot;
                    }
                    NftExpression::Meta { key, dreg }
                }
                NftExpressionInput::MetaSet { key: 3, sreg } => {
                    let sreg = data_register(*sreg, 4)?;
                    if initialized & (1 << (sreg / 4)) == 0 {
                        return Err(SystemError::ENODATA);
                    }
                    NftExpression::MetaSetMark { sreg }
                }
                NftExpressionInput::MetaSet { .. } => {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
                }
                NftExpressionInput::Fib {
                    dreg,
                    result: 3,
                    flags: 2,
                } if family == 2 => {
                    let dreg = data_register(*dreg, 4)?;
                    initialized |= 1 << (dreg / 4);
                    NftExpression::FibDaddrType { dreg }
                }
                NftExpressionInput::Fib { .. } => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                NftExpressionInput::ImmediateData { dreg, data } => {
                    let dreg = data_register(*dreg, data.len())?;
                    let first = dreg / 4;
                    let last = (dreg + data.len() - 1) / 4;
                    for slot in first..=last {
                        initialized |= 1 << slot;
                    }
                    NftExpression::ImmediateData {
                        dreg,
                        data: copy_bytes(data)?,
                    }
                }
                NftExpressionInput::Lookup {
                    set,
                    set_id,
                    sreg,
                    dreg,
                    invert,
                } => {
                    let by_name =
                        set.and_then(|name| table.sets.iter().find(|item| item.name == name));
                    let by_id = set_id.and_then(|id| {
                        self.pending_set_ids
                            .iter()
                            .find(|entry| {
                                entry.0 == family && entry.1 == table_name && entry.2 == id
                            })
                            .and_then(|entry| table.sets.iter().find(|item| item.handle == entry.3))
                    });
                    if by_name.is_some()
                        && by_id.is_some()
                        && by_name.unwrap().handle != by_id.unwrap().handle
                    {
                        return Err(SystemError::EINVAL);
                    }
                    let set = by_name.or(by_id).ok_or(SystemError::ENOENT)?;
                    let verdict_map = set.data_type == Some(0xffff_ff00);
                    if verdict_map && index + 1 != inputs.len() {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let dreg = match (set.data_len, dreg) {
                        (Some(0), Some(0)) if verdict_map && !*invert => Some(0),
                        (Some(len), Some(dreg)) if !verdict_map && !*invert => {
                            Some(data_register(*dreg, len)?)
                        }
                        (None, None) => None,
                        _ => return Err(SystemError::EINVAL),
                    };
                    let sreg = data_register(*sreg, set.key_len)?;
                    let first = sreg / 4;
                    let last = (sreg + set.key_len - 1) / 4;
                    if (first..=last).any(|slot| initialized & (1 << slot) == 0) {
                        return Err(SystemError::ENODATA);
                    }
                    if let (Some(start), Some(len)) = (dreg, set.data_len) {
                        if !verdict_map {
                            for slot in start / 4..=(start + len - 1) / 4 {
                                initialized |= 1 << slot;
                            }
                        }
                    }
                    NftExpression::Lookup {
                        set_handle: set.handle,
                        sreg,
                        key_len: set.key_len,
                        dreg,
                        verdict_map,
                        invert: *invert,
                    }
                }
                NftExpressionInput::Immediate(input) => {
                    // Once a verdict is reached, later expressions cannot
                    // execute. Reject that layout until full Linux rule
                    // reduction semantics are supported.
                    if index + 1 != inputs.len() {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let verdict = match input {
                        NftRuleInput::Accept => NftRuleVerdict::Accept,
                        NftRuleInput::Drop => NftRuleVerdict::Drop,
                        NftRuleInput::Continue => NftRuleVerdict::Continue,
                        NftRuleInput::Return => NftRuleVerdict::Return,
                        NftRuleInput::Jump(name) | NftRuleInput::Goto(name) => {
                            let target = table
                                .chains
                                .iter()
                                .find(|candidate| candidate.name == *name)
                                .ok_or(SystemError::ENOENT)?;
                            if target.base.is_some() {
                                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                            }
                            if matches!(input, NftRuleInput::Jump(_)) {
                                NftRuleVerdict::Jump(target.handle)
                            } else {
                                NftRuleVerdict::Goto(target.handle)
                            }
                        }
                    };
                    NftExpression::Immediate(verdict)
                }
                NftExpressionInput::Payload {
                    dreg,
                    base,
                    offset,
                    len,
                } => {
                    let base = match *base {
                        1 => NftPayloadBase::Network,
                        2 => NftPayloadBase::Transport,
                        _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                    };
                    if *offset > 255 {
                        return Err(SystemError::ERANGE);
                    }
                    let len = usize::try_from(*len).map_err(|_| SystemError::ERANGE)?;
                    let dreg = data_register(*dreg, len)?;
                    let first = dreg / 4;
                    let last = (dreg + len - 1) / 4;
                    for slot in first..=last {
                        initialized |= 1 << slot;
                    }
                    NftExpression::Payload {
                        base,
                        dreg,
                        offset: *offset as usize,
                        len,
                    }
                }
                NftExpressionInput::Cmp { sreg, op, data } => {
                    let sreg = data_register(*sreg, data.len())?;
                    let first = sreg / 4;
                    let last = (sreg + data.len() - 1) / 4;
                    if (first..=last).any(|slot| initialized & (1 << slot) == 0) {
                        return Err(SystemError::ENODATA);
                    }
                    let op = match op {
                        0 => NftCmpOp::Eq,
                        1 => NftCmpOp::Neq,
                        2 => NftCmpOp::Lt,
                        3 => NftCmpOp::Lte,
                        4 => NftCmpOp::Gt,
                        5 => NftCmpOp::Gte,
                        _ => return Err(SystemError::EINVAL),
                    };
                    NftExpression::Cmp {
                        sreg,
                        op,
                        data: copy_bytes(data)?,
                    }
                }
                NftExpressionInput::Byteorder {
                    sreg,
                    dreg,
                    op,
                    len,
                    size,
                } => {
                    if *len > u8::MAX as u32 || *size > u8::MAX as u32 {
                        return Err(SystemError::ERANGE);
                    }
                    let op = match op {
                        0 => NftByteorderOp::NetworkToHost,
                        1 => NftByteorderOp::HostToNetwork,
                        _ => return Err(SystemError::EINVAL),
                    };
                    let size = match size {
                        2 | 4 | 8 => *size as usize,
                        _ => return Err(SystemError::EINVAL),
                    };
                    let len = *len as usize;
                    let sreg = data_register(*sreg, len)?;
                    let dreg = data_register(*dreg, len)?;
                    if (sreg / 4..=(sreg + len - 1) / 4).any(|slot| initialized & (1 << slot) == 0)
                    {
                        return Err(SystemError::ENODATA);
                    }
                    let written = len / size * size;
                    if written != 0 {
                        for slot in dreg / 4..=(dreg + written - 1) / 4 {
                            initialized |= 1 << slot;
                        }
                    }
                    NftExpression::Byteorder {
                        sreg,
                        dreg,
                        op,
                        len,
                        size,
                    }
                }
                NftExpressionInput::Range { sreg, op, from, to } => {
                    if from.len() != to.len() {
                        return Err(SystemError::EINVAL);
                    }
                    let sreg = data_register(*sreg, from.len())?;
                    if (sreg / 4..=(sreg + from.len() - 1) / 4)
                        .any(|slot| initialized & (1 << slot) == 0)
                    {
                        return Err(SystemError::ENODATA);
                    }
                    let op = match op {
                        0 => NftRangeOp::Eq,
                        1 => NftRangeOp::Neq,
                        _ => return Err(SystemError::EINVAL),
                    };
                    NftExpression::Range {
                        sreg,
                        op,
                        from: copy_bytes(from)?,
                        to: copy_bytes(to)?,
                    }
                }
                NftExpressionInput::Bitwise {
                    sreg,
                    dreg,
                    len,
                    op,
                    mask,
                    xor,
                    data,
                } => {
                    if *len > u8::MAX as u32 {
                        return Err(SystemError::ERANGE);
                    }
                    let len = usize::try_from(*len).map_err(|_| SystemError::ERANGE)?;
                    let sreg = data_register(*sreg, len)?;
                    let dreg = data_register(*dreg, len)?;
                    let first = sreg / 4;
                    let last = (sreg + len - 1) / 4;
                    if (first..=last).any(|slot| initialized & (1 << slot) == 0) {
                        return Err(SystemError::ENODATA);
                    }
                    let operation = match *op {
                        0 => {
                            if data.is_some() {
                                return Err(SystemError::EINVAL);
                            }
                            let mask = mask.ok_or(SystemError::EINVAL)?;
                            let xor = xor.ok_or(SystemError::EINVAL)?;
                            if mask.len() != len || xor.len() != len {
                                return Err(SystemError::EINVAL);
                            }
                            NftBitwiseOperation::Bool {
                                mask: copy_bytes(mask)?,
                                xor: copy_bytes(xor)?,
                            }
                        }
                        1 | 2 => {
                            if mask.is_some() || xor.is_some() {
                                return Err(SystemError::EINVAL);
                            }
                            let data = data.ok_or(SystemError::EINVAL)?;
                            if data.len() != 4 {
                                return Err(SystemError::EINVAL);
                            }
                            let shift = u32::from_ne_bytes(data.try_into().unwrap());
                            if shift >= 32 {
                                return Err(SystemError::EINVAL);
                            }
                            if *op == 1 {
                                NftBitwiseOperation::Lshift(shift)
                            } else {
                                NftBitwiseOperation::Rshift(shift)
                            }
                        }
                        _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                    };
                    let first = dreg / 4;
                    let last = (dreg + len - 1) / 4;
                    for slot in first..=last {
                        initialized |= 1 << slot;
                    }
                    NftExpression::Bitwise {
                        sreg,
                        dreg,
                        len,
                        operation,
                    }
                }
                NftExpressionInput::Counter { bytes, packets } => {
                    NftExpression::Counter(NftCounter::new(*bytes, *packets)?)
                }
            };
            expressions.push(expression);
        }
        let rule = Arc::try_new(NftRule {
            handle,
            expressions,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let mut pending_ct_hook_order = self.pending_ct_hook_order;
        let mut next_hook_order = self.candidate.next_hook_order;
        if rule.requires_conntrack() {
            for (index, applies) in [matches!(family, 1 | 2), matches!(family, 1 | 10)]
                .into_iter()
                .enumerate()
            {
                if applies
                    && self.candidate.conntrack_hook_order[index].is_none()
                    && pending_ct_hook_order[index].is_none()
                {
                    pending_ct_hook_order[index] = Some(next_hook_order);
                    next_hook_order = next_hook_order.checked_add(1).ok_or(SystemError::ENOSPC)?;
                }
            }
        }
        let validate_calls = matches!(
            rule.verdict(),
            NftRuleVerdict::Jump(_) | NftRuleVerdict::Goto(_)
        );
        let table = self.table_for_write(table_index)?;
        if Arc::get_mut(&mut table.chains[chain_index]).is_none() {
            table.chains[chain_index] = table.chains[chain_index].clone_for_write()?;
        }
        let chain = Arc::get_mut(&mut table.chains[chain_index]).unwrap();
        chain
            .rules
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        chain.rules.insert(insertion_index, rule.clone());
        if validate_calls {
            // Linux validates a new verdict's reachable call graph while
            // processing NEWRULE. Report an invalid edge on that request,
            // not as a later batch-commit error on BATCH_BEGIN.
            if let Err(error) = table.validate_reachable_chains() {
                Arc::get_mut(&mut table.chains[chain_index])
                    .unwrap()
                    .rules
                    .remove(insertion_index);
                return Err(error);
            }
        }
        let chain = table.chains[chain_index].clone();
        table.next_handle = next_handle;
        self.pending_ct_hook_order = pending_ct_hook_order;
        self.candidate.next_hook_order = next_hook_order;
        self.changed = true;
        Ok((self.candidate.tables[table_index].clone(), chain, rule))
    }

    pub(crate) fn del_rules(
        &mut self,
        family: u8,
        table_name: &[u8],
        chain_name: Option<&[u8]>,
        handle: Option<u64>,
    ) -> Result<Vec<CreatedRule>, SystemError> {
        let table_index = self
            .candidate
            .tables
            .iter()
            .position(|table| table.family == family && table.name == table_name)
            .ok_or(SystemError::ENOENT)?;
        let original = self.candidate.tables[table_index].clone();
        let chain_index = chain_name
            .map(|name| {
                original
                    .chains
                    .iter()
                    .position(|chain| chain.name == name)
                    .ok_or(SystemError::ENOENT)
            })
            .transpose()?;
        let rule_index = if let (Some(chain_index), Some(handle)) = (chain_index, handle) {
            Some(
                original.chains[chain_index]
                    .rules
                    .iter()
                    .position(|rule| rule.handle == handle)
                    .ok_or(SystemError::ENOENT)?,
            )
        } else {
            None
        };
        let count = if rule_index.is_some() {
            1
        } else if let Some(chain_index) = chain_index {
            original.chains[chain_index].rules.len()
        } else {
            original
                .chains
                .iter()
                .try_fold(0usize, |sum, chain| sum.checked_add(chain.rules.len()))
                .ok_or(SystemError::ENOMEM)?
        };
        let mut removed = Vec::new();
        removed
            .try_reserve(count)
            .map_err(|_| SystemError::ENOMEM)?;
        if count == 0 {
            return Ok(removed);
        }
        let table = self.table_for_write(table_index)?;
        for index in 0..table.chains.len() {
            if chain_index.is_some_and(|selected| selected != index)
                || table.chains[index].rules.is_empty()
            {
                continue;
            }
            let prior_chain = table.chains[index].clone();
            if Arc::get_mut(&mut table.chains[index]).is_none() {
                table.chains[index] = table.chains[index].clone_for_write()?;
            }
            let chain = Arc::get_mut(&mut table.chains[index]).unwrap();
            if let Some(rule_index) = rule_index {
                removed.push((
                    original.clone(),
                    prior_chain,
                    chain.rules.remove(rule_index),
                ));
            } else {
                for rule in core::mem::take(&mut chain.rules) {
                    removed.push((original.clone(), prior_chain.clone(), rule));
                }
            }
        }
        let chains = &table.chains;
        let mut reclaimed = 0usize;
        table.sets.retain(|set| {
            let removed_binding = removed.iter().any(|(_, _, rule)| {
                rule.expressions.iter().any(|expression| {
                    matches!(expression, NftExpression::Lookup { set_handle, .. }
                        if *set_handle == set.handle)
                })
            });
            let still_bound = chains
                .iter()
                .flat_map(|chain| chain.rules.iter())
                .any(|rule| {
                    rule.expressions.iter().any(|expression| {
                        matches!(expression, NftExpression::Lookup { set_handle, .. }
                        if *set_handle == set.handle)
                    })
                });
            let keep = set.flags & 1 == 0 || !removed_binding || still_bound;
            if !keep {
                reclaimed += 1;
            }
            keep
        });
        table.use_count -= reclaimed as u32;
        self.changed = true;
        Ok(removed)
    }

    pub(crate) fn del_table(
        &mut self,
        family: u8,
        name: Option<&[u8]>,
        handle: Option<u64>,
        non_recursive: bool,
    ) -> Result<Vec<Arc<NftTable>>, SystemError> {
        if family == 0 || (name.is_none() && handle.is_none()) {
            let mut removed = Vec::new();
            removed
                .try_reserve(self.candidate.tables.len())
                .map_err(|_| SystemError::ENOMEM)?;
            self.candidate.tables.retain(|table| {
                let retain = (family != 0 && table.family != family)
                    || name.is_some_and(|name| table.name != name);
                if !retain {
                    removed.push(table.clone());
                }
                retain
            });
            self.changed |= !removed.is_empty();
            return Ok(removed);
        }
        let index = self
            .candidate
            .tables
            .iter()
            .position(|table| {
                table.family == family
                    && handle.map_or_else(
                        || name.is_some_and(|name| table.name == name),
                        |handle| table.handle == handle,
                    )
            })
            .ok_or(SystemError::ENOENT)?;
        if non_recursive && self.candidate.tables[index].use_count != 0 {
            return Err(SystemError::EBUSY);
        }
        let mut removed = Vec::new();
        removed.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        removed.push(self.candidate.tables.remove(index));
        self.changed = true;
        Ok(removed)
    }

    /// Publish and notify under the writer lock so observers see commit-order
    /// notifications even when independent writers race in one namespace.
    pub(crate) fn commit(
        self,
        netns: &NetNamespace,
        notify: impl FnOnce(u32),
    ) -> Result<bool, SystemError> {
        let mut this = self;
        if !this.changed {
            return Ok(false);
        }
        for table in &this.candidate.tables {
            for set in table.sets.iter().filter(|set| set.flags & 1 != 0) {
                if !table
                    .chains
                    .iter()
                    .flat_map(|chain| chain.rules.iter())
                    .any(|rule| {
                        rule.expressions.iter().any(|expression| {
                            matches!(expression, NftExpression::Lookup { set_handle, .. }
                            if *set_handle == set.handle)
                        })
                    })
                {
                    return Err(SystemError::EINVAL);
                }
            }
            table.validate_reachable_chains()?;
        }
        let new_families = this
            .candidate
            .reconcile_conntrack_hooks(this.pending_ct_hook_order)?;
        this.candidate
            .reconcile_nat_hooks(this.pending_nat_outer_hook_order)?;
        let retire = PreparedRcuArcRetire::prepare().map_err(|_| SystemError::ENOMEM)?;
        this.candidate.compile_ipv4_hooks()?;
        this.candidate.requires_route_lookup = this
            .candidate
            .tables
            .iter()
            .any(|table| table.requires_route_lookup());
        this.candidate.generation = this.candidate.generation.wrapping_add(1);
        if this.candidate.generation == 0 {
            this.candidate.generation = 1;
        }
        let generation = this.candidate.generation;
        let candidate = Arc::try_new(this.candidate).map_err(|_| SystemError::ENOMEM)?;
        // The old snapshot cannot reach tracking until the new one is
        // published. Prepare both families before activating either so an
        // allocation failure leaves the ruleset and packet path unchanged.
        if new_families[0] {
            netns.prepare_ipv4_defrag()?;
        }
        if new_families[1] {
            netns.prepare_ipv6_defrag()?;
        }
        let ct = netns.conntrack();
        match new_families {
            [true, true] => ct.activate(CtState::DEFAULT_MAX_FLOWS),
            [true, false] => ct.activate_family(IpVersion::Ipv4, CtState::DEFAULT_MAX_FLOWS),
            [false, true] => ct.activate_family(IpVersion::Ipv6, CtState::DEFAULT_MAX_FLOWS),
            [false, false] => Ok(()),
        }
        .map_err(|error| match error {
            CtError::NoMemory => SystemError::ENOMEM,
            CtError::Full => SystemError::ENOSPC,
            _ => SystemError::EINVAL,
        })?;
        if new_families[0] {
            netns.enable_ipv4_defrag()?;
        }
        if new_families[1] {
            netns.enable_ipv6_defrag()?;
        }
        let old = this.state.ruleset.swap_prepared(candidate, retire);
        notify(generation);
        drop(this.writer);
        old.enqueue();
        Ok(true)
    }
}
