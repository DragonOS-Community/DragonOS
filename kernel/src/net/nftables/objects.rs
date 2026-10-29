use super::*;

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
    pub(super) next_handle: u64,
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
    pub(super) fn element_position(&self, key: &[u8], flags: u32) -> Result<usize, usize> {
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

    pub(super) fn clone_for_write(&self) -> Result<Arc<Self>, SystemError> {
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
    pub(super) rules: Vec<Arc<NftRule>>,
}

pub(crate) type CreatedChain = (Arc<NftTable>, Arc<NftChain>);
pub(crate) type CreatedRule = (Arc<NftTable>, Arc<NftChain>, Arc<NftRule>);

/// A regular chain has no packet hook or fallthrough policy. Keeping these
/// fields together prevents it from being registered as a base chain when
/// regular chains become available to the nftables control plane.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NftBaseChain {
    pub(super) hook_order: u64,
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
    pub(super) const COUNT: usize = 5;

    pub(super) const fn index(self) -> usize {
        self as usize
    }
}

impl NftChain {
    pub(crate) fn rules(&self) -> &[Arc<NftRule>] {
        &self.rules
    }

    pub(super) fn clone_for_write(&self) -> Result<Arc<Self>, SystemError> {
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

pub(super) fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, SystemError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| SystemError::ENOMEM)?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

pub(super) fn allocated_set_name(
    table: &NftTable,
    template: &[u8],
) -> Result<Vec<u8>, SystemError> {
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

pub(super) fn verdict_target(verdict: NftRuleVerdict) -> Option<u64> {
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

    pub(super) fn requires_route_lookup(&self) -> bool {
        self.chains
            .iter()
            .filter(|chain| chain.base.is_some())
            .any(|chain| self.chain_requires_route_lookup(chain, 0))
    }

    pub(super) fn chain_requires_iface_names(&self, chain: &NftChain, depth: usize) -> bool {
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

    pub(super) fn chain_requires_route_lookup(&self, chain: &NftChain, depth: usize) -> bool {
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

    pub(super) fn chain_requires_redirect(&self, chain: &NftChain, depth: usize) -> bool {
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

    pub(super) fn validate_reachable_chains(&self) -> Result<(), SystemError> {
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

    pub(super) fn inbound_references(&self, handle: u64) -> usize {
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

    pub(super) fn evaluate_base_chain(
        &self,
        base_handle: u64,
        packet: &NftPacket<'_>,
    ) -> NftVerdict {
        match self.evaluate_base_chain_result(base_handle, packet) {
            ChainResult::Verdict(verdict) => verdict,
            // A NAT action at a filter hook is invalid and must fail closed.
            ChainResult::Nat(_) => NftVerdict::Drop,
        }
    }

    pub(super) fn evaluate_base_chain_result(
        &self,
        base_handle: u64,
        packet: &NftPacket<'_>,
    ) -> ChainResult {
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

    pub(super) fn clone_for_write(&self) -> Result<Arc<Self>, SystemError> {
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
