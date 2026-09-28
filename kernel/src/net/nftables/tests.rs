use super::*;

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
