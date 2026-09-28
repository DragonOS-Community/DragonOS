use super::*;

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
