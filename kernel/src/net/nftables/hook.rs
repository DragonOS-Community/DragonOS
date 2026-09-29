use super::*;

/// One immutable hook program is published with the ruleset snapshot. The
/// fixed conntrack step is ordered with user chains, not run before raw chains.
#[derive(Debug)]
pub(super) struct HookProgram {
    steps: Vec<HookStep>,
}

#[derive(Debug)]
pub(super) enum HookStep {
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
    pub(super) fn empty() -> Self {
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

    fn postrouting_nat_snapshot(expression: NftExpression, outer_hook: bool) -> RulesetSnapshot {
        let mut table = table_with_chain(2, 1, 100, 1, NftVerdict::Accept, None);
        Arc::get_mut(&mut table).unwrap().name = b"nat".to_vec();
        let chain = Arc::get_mut(&mut Arc::get_mut(&mut table).unwrap().chains[0]).unwrap();
        let base = chain.base.as_mut().unwrap();
        base.chain_type = NftChainType::Nat;
        base.hook = NftIpv4Hook::PostRouting;
        chain.rules.push(Arc::new(NftRule {
            handle: 2,
            expressions: alloc::vec![expression],
        }));
        assert!(table.validate_reachable_chains().is_ok());
        let mut ruleset = snapshot(alloc::vec![table]);
        ruleset.nat_outer_hook_order[0] = outer_hook.then_some(1);
        ruleset.compile_ipv4_hooks().unwrap();
        ruleset
    }

    #[test]
    fn xt_masquerade_requires_the_same_address_snapshot_as_native_masquerade() {
        let mut xt_info = [0u8; 24];
        xt_info[..4].copy_from_slice(&1u32.to_ne_bytes());
        let xt_action = parse_xt_nat_target(2, b"MASQUERADE", 0, &xt_info).unwrap();
        let xt = || NftExpression::XtNatTarget {
            name: b"MASQUERADE",
            revision: 0,
            info: xt_info.to_vec(),
            action: xt_action,
        };
        let xt_ruleset = postrouting_nat_snapshot(xt(), true);
        assert!(xt_ruleset.requires_masquerade(IpVersion::Ipv4, NftIpv4Hook::PostRouting));
        assert!(!xt_ruleset.requires_masquerade(IpVersion::Ipv6, NftIpv4Hook::PostRouting));
        assert!(!xt_ruleset.requires_masquerade(IpVersion::Ipv4, NftIpv4Hook::PreRouting));

        let native = postrouting_nat_snapshot(
            NftExpression::Masquerade {
                port_min: None,
                port_max: None,
            },
            true,
        );
        assert!(native.requires_masquerade(IpVersion::Ipv4, NftIpv4Hook::PostRouting));
        assert!(!postrouting_nat_snapshot(xt(), false)
            .requires_masquerade(IpVersion::Ipv4, NftIpv4Hook::PostRouting));

        let mut snat_info = [0u8; 24];
        snat_info[..4].copy_from_slice(&1u32.to_ne_bytes());
        snat_info[4..8].copy_from_slice(&1u32.to_ne_bytes());
        snat_info[8..12].copy_from_slice(&[192, 0, 2, 1]);
        snat_info[12..16].copy_from_slice(&[192, 0, 2, 1]);
        let snat = NftExpression::XtNatTarget {
            name: b"SNAT",
            revision: 0,
            info: snat_info.to_vec(),
            action: parse_xt_nat_target(2, b"SNAT", 0, &snat_info).unwrap(),
        };
        assert!(!postrouting_nat_snapshot(snat, true)
            .requires_masquerade(IpVersion::Ipv4, NftIpv4Hook::PostRouting));
    }

    #[test]
    fn xt_masquerade_in_a_jumped_chain_still_requires_address_snapshot() {
        let mut info = [0u8; 24];
        info[..4].copy_from_slice(&1u32.to_ne_bytes());
        let mut table = table_with_chain(2, 1, 100, 1, NftVerdict::Accept, None);
        let inner = Arc::get_mut(&mut table).unwrap();
        inner.name = b"nat".to_vec();
        let base = Arc::get_mut(&mut inner.chains[0]).unwrap();
        base.base.as_mut().unwrap().chain_type = NftChainType::Nat;
        base.base.as_mut().unwrap().hook = NftIpv4Hook::PostRouting;
        base.rules.push(Arc::new(NftRule {
            handle: 2,
            expressions: alloc::vec![NftExpression::Immediate(NftRuleVerdict::Jump(3))],
        }));
        inner.chains.push(Arc::new(NftChain {
            name: b"masq".to_vec(),
            handle: 3,
            base: None,
            rules: alloc::vec![Arc::new(NftRule {
                handle: 4,
                expressions: alloc::vec![NftExpression::XtNatTarget {
                    name: b"MASQUERADE",
                    revision: 0,
                    info: info.to_vec(),
                    action: parse_xt_nat_target(2, b"MASQUERADE", 0, &info).unwrap(),
                }],
            })],
        }));
        assert!(table.validate_reachable_chains().is_ok());
        let mut ruleset = snapshot(alloc::vec![table]);
        ruleset.nat_outer_hook_order[0] = Some(1);
        ruleset.compile_ipv4_hooks().unwrap();
        assert!(ruleset.requires_masquerade(IpVersion::Ipv4, NftIpv4Hook::PostRouting));
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
    pub(super) next_hook_order: u64,
    /// Independent IPv4/IPv6 registration sequences for this snapshot.
    /// The last consumer removes its family's hook; a later rule registers
    /// anew with a later sequence, as in Linux.
    pub(super) conntrack_hook_order: [Option<u64>; 2],
    /// Linux registers all four fixed NAT outer hooks for a protocol when
    /// its first NAT base chain appears, retaining their registration order
    /// until the last base chain for that protocol is removed.
    pub(super) nat_outer_hook_order: [Option<u64>; 2],
    pub(super) requires_route_lookup: bool,
    pub(super) requires_iface_names: bool,
    pub(super) iface_name_hooks: [bool; NftIpv4Hook::COUNT],
    pub(super) ipv4_route_lookup_hooks: [bool; NftIpv4Hook::COUNT],
    pub(super) ipv6_iface_name_hooks: [bool; NftIpv4Hook::COUNT],
    pub(super) ipv6_local_destination_hooks: [bool; NftIpv4Hook::COUNT],
    /// Hook order is compiled once when the candidate becomes immutable.
    /// Packet evaluation never sorts or takes the writer lock.
    pub(super) ipv4_hooks: [HookProgram; NftIpv4Hook::COUNT],
    pub(super) ipv6_hooks: [HookProgram; NftIpv4Hook::COUNT],
}

impl RulesetSnapshot {
    pub(super) fn reconcile_nat_hooks(
        &mut self,
        pending: [Option<u64>; 2],
    ) -> Result<(), SystemError> {
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
    pub(super) fn reconcile_conntrack_hooks(
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
                            matches!(
                                expression,
                                NftExpression::Masquerade { .. }
                                    | NftExpression::XtNatTarget {
                                        action: NftNatAction::Masquerade { .. },
                                        ..
                                    }
                            )
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

    pub(super) fn compile_ipv4_hooks(&mut self) -> Result<(), SystemError> {
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
