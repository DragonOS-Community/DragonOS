//! The IPv6 routing policy database. Rule ordering is independent of the
//! prefix and alias ordering inside an individual FIB table.

use alloc::vec::Vec;
use smoltcp::wire::{Ipv6Address, Ipv6Cidr};
use system_error::SystemError;

use super::{RTPROT_KERNEL, RT_TABLE_LOCAL, RT_TABLE_MAIN};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::net) struct Ipv6Rule {
    pub priority: u32,
    pub table: u32,
    pub source: Option<Ipv6Cidr>,
    pub protocol: u8,
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::net) struct Ipv6RuleSelector {
    pub priority: Option<u32>,
    pub table: Option<u32>,
    pub source: Option<Ipv6Cidr>,
    pub protocol: Option<u8>,
}

const DEFAULT_RULES: &[Ipv6Rule] = &[
    Ipv6Rule {
        priority: 0,
        table: RT_TABLE_LOCAL,
        source: None,
        protocol: RTPROT_KERNEL,
    },
    Ipv6Rule {
        priority: 32766,
        table: RT_TABLE_MAIN,
        source: None,
        protocol: RTPROT_KERNEL,
    },
];

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Ipv6Policy {
    /// An unedited namespace reads the defaults without allocating a list.
    rules: Option<Vec<Ipv6Rule>>,
    /// Linux enables the dynamic lookup path after successful configuration.
    /// Before then, deleting a default affects dumps but not the fast lookup.
    active: bool,
    has_source_rules: bool,
    generation: u64,
}

impl Ipv6Policy {
    pub(super) fn rules(&self) -> &[Ipv6Rule] {
        self.rules.as_deref().unwrap_or(DEFAULT_RULES)
    }

    pub(super) fn changes_table_order(&self) -> bool {
        self.active && self.rules() != DEFAULT_RULES
    }

    pub(super) fn has_source_rules(&self) -> bool {
        self.has_source_rules
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn lookup_tables(
        &self,
        source: Option<Ipv6Address>,
    ) -> impl Iterator<Item = u32> + '_ {
        let rules = if self.active {
            self.rules()
        } else {
            DEFAULT_RULES
        };
        rules.iter().filter_map(move |rule| {
            rule.source
                .is_none_or(|prefix| source.is_some_and(|address| prefix.contains_addr(&address)))
                .then_some(rule.table)
        })
    }

    pub(super) fn insert(
        &mut self,
        request: Ipv6RuleSelector,
        exclusive: bool,
    ) -> Result<Ipv6Rule, SystemError> {
        let table = request.table.ok_or(SystemError::EINVAL)?;
        let priority = request.priority.unwrap_or_else(|| {
            self.rules()
                .get(1)
                .map_or(0, |rule| rule.priority.saturating_sub(1))
        });
        let rule = Ipv6Rule {
            table,
            priority,
            source: request.source,
            protocol: request.protocol.unwrap_or_default(),
        };
        if exclusive
            && self.rules().iter().any(|existing| {
                existing.table == table
                    && existing.priority == priority
                    && existing.protocol == rule.protocol
                    && rule
                        .source
                        .is_none_or(|source| existing.source == Some(source))
            })
        {
            return Err(SystemError::EEXIST);
        }

        let index = self
            .rules()
            .partition_point(|existing| existing.priority <= priority);
        let rules = self.editable_rules(1)?;
        rules.insert(index, rule);
        self.active = true;
        self.has_source_rules |= rule.source.is_some();
        self.generation += 1;
        Ok(rule)
    }

    pub(super) fn remove(&mut self, selector: Ipv6RuleSelector) -> Result<Ipv6Rule, SystemError> {
        let index = self
            .rules()
            .iter()
            .position(|rule| {
                selector
                    .priority
                    .is_none_or(|priority| rule.priority == priority)
                    && selector.table.is_none_or(|table| rule.table == table)
                    && selector
                        .source
                        .is_none_or(|source| rule.source == Some(source))
                    && selector.protocol.is_none_or(|proto| rule.protocol == proto)
            })
            .ok_or(SystemError::ENOENT)?;
        let rule = self.editable_rules(0)?.remove(index);
        if rule.source.is_some() {
            self.has_source_rules = self.rules().iter().any(|rule| rule.source.is_some());
        }
        self.generation += 1;
        Ok(rule)
    }

    fn editable_rules(&mut self, extra: usize) -> Result<&mut Vec<Ipv6Rule>, SystemError> {
        if self.rules.is_none() {
            let mut rules = Vec::new();
            rules
                .try_reserve(DEFAULT_RULES.len() + extra)
                .map_err(|_| SystemError::ENOMEM)?;
            rules.extend_from_slice(DEFAULT_RULES);
            return Ok(self.rules.insert(rules));
        }
        let rules = self.rules.as_mut().unwrap();
        rules.try_reserve(extra).map_err(|_| SystemError::ENOMEM)?;
        Ok(rules)
    }

    pub(super) fn try_clone(&self) -> Result<Self, SystemError> {
        let rules = if let Some(source) = &self.rules {
            let mut rules = Vec::new();
            rules
                .try_reserve(source.len())
                .map_err(|_| SystemError::ENOMEM)?;
            rules.extend_from_slice(source);
            Some(rules)
        } else {
            None
        };
        Ok(Self {
            rules,
            active: self.active,
            has_source_rules: self.has_source_rules,
            generation: self.generation,
        })
    }
}
