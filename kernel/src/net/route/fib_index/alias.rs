use alloc::vec::Vec;

use hashbrown::HashMap;
use smoltcp::wire::IpAddress;
use system_error::SystemError;

use super::PrefixKey;
use crate::net::route::RouteEntry;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct ConflictKey {
    prefix: PrefixKey,
    priority: u32,
    ipv4_tos: Option<u8>,
}

impl ConflictKey {
    fn new(route: RouteEntry) -> Self {
        Self {
            prefix: PrefixKey::new(route.table, route.destination),
            priority: route.priority,
            ipv4_tos: matches!(route.destination.address(), IpAddress::Ipv4(_))
                .then_some(route.tos),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConflictGroup {
    order: u128,
    head: i128,
    tail: i128,
    first: usize,
    last: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::net::route) struct AliasOrder {
    priority: u32,
    group: u128,
    member: i128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::net::route) enum AliasPlacement {
    NewGroup,
    Prepend,
    Append,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::net::route) struct AliasInsertAnalysis {
    pub(in crate::net::route) first: Option<usize>,
    pub(in crate::net::route) exact: Option<usize>,
    pub(in crate::net::route) placement: AliasPlacement,
}

/// Write-side metadata for Linux-style aliases sharing one destination prefix.
///
/// The authoritative route vector remains unchanged. Hash lookups answer the
/// exact/conflict questions used by insertion planning, while a compact order
/// key preserves metric/group/prepend/append precedence across `swap_remove`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct AliasIndex {
    exact: HashMap<RouteEntry, usize>,
    groups: HashMap<ConflictKey, ConflictGroup>,
    order: Vec<AliasOrder>,
    next_group: u128,
}

impl AliasIndex {
    pub(super) fn try_clone(&self) -> Result<Self, SystemError> {
        Ok(Self {
            exact: try_clone_map(&self.exact)?,
            groups: try_clone_map(&self.groups)?,
            order: try_clone_vec(&self.order)?,
            next_group: self.next_group,
        })
    }

    pub(super) fn prepare_insert(&mut self, route: RouteEntry) -> Result<(), SystemError> {
        self.order.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        self.exact.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        if !self.groups.contains_key(&ConflictKey::new(route)) {
            self.groups
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        Ok(())
    }

    pub(super) fn analyze_insert(&self, route: RouteEntry) -> AliasInsertAnalysis {
        let group = self.groups.get(&ConflictKey::new(route));
        AliasInsertAnalysis {
            first: group.map(|group| group.first),
            exact: self.exact.get(&route).copied(),
            placement: if group.is_some() {
                AliasPlacement::Prepend
            } else {
                AliasPlacement::NewGroup
            },
        }
    }

    pub(super) fn append_placement(&self, route: RouteEntry) -> AliasPlacement {
        if self.groups.contains_key(&ConflictKey::new(route)) {
            AliasPlacement::Append
        } else {
            AliasPlacement::NewGroup
        }
    }

    pub(super) fn planned_order(&self, route: RouteEntry, placement: AliasPlacement) -> AliasOrder {
        let group = self.groups.get(&ConflictKey::new(route));
        let (group, member) = match placement {
            AliasPlacement::NewGroup => (self.next_group, 0),
            AliasPlacement::Prepend => {
                let group = group.expect("prepend requires an existing conflict group");
                (
                    group.order,
                    group
                        .head
                        .checked_sub(1)
                        .expect("route alias prepend order cannot overflow i128"),
                )
            }
            AliasPlacement::Append => {
                let group = group.expect("append requires an existing conflict group");
                (
                    group.order,
                    group
                        .tail
                        .checked_add(1)
                        .expect("route alias append order cannot overflow i128"),
                )
            }
        };
        AliasOrder {
            priority: route.priority,
            group,
            member,
        }
    }

    pub(super) fn commit_insert(
        &mut self,
        route_index: usize,
        route: RouteEntry,
        placement: AliasPlacement,
    ) {
        let key = ConflictKey::new(route);
        let order = self.planned_order(route, placement);
        match placement {
            AliasPlacement::NewGroup => {
                self.next_group = self
                    .next_group
                    .checked_add(1)
                    .expect("route alias group order cannot overflow u128");
                self.groups.insert(
                    key,
                    ConflictGroup {
                        order: order.group,
                        head: order.member,
                        tail: order.member,
                        first: route_index,
                        last: route_index,
                    },
                );
            }
            AliasPlacement::Prepend => {
                let group = self.groups.get_mut(&key).unwrap();
                group.head = order.member;
                group.first = route_index;
            }
            AliasPlacement::Append => {
                let group = self.groups.get_mut(&key).unwrap();
                group.tail = order.member;
                group.last = route_index;
            }
        }
        debug_assert_eq!(route_index, self.order.len());
        self.order.push(order);
        self.exact.insert(route, route_index);
    }

    pub(super) fn commit_remove(
        &mut self,
        route_index: usize,
        route: RouteEntry,
        remaining_prefix: &[usize],
    ) {
        self.exact.remove(&route);
        let key = ConflictKey::new(route);
        let group_order = self.order[route_index].group;
        let mut first: Option<usize> = None;
        let mut last: Option<usize> = None;
        for index in remaining_prefix
            .iter()
            .copied()
            .filter(|index| self.order[*index].group == group_order)
        {
            if first.is_none_or(|current| self.order[index] < self.order[current]) {
                first = Some(index);
            }
            if last.is_none_or(|current| self.order[index] > self.order[current]) {
                last = Some(index);
            }
        }
        match (first, last) {
            (Some(first), Some(last)) => {
                let group = self.groups.get_mut(&key).unwrap();
                group.first = first;
                group.last = last;
                group.head = self.order[first].member;
                group.tail = self.order[last].member;
            }
            (None, None) => {
                self.groups.remove(&key);
            }
            _ => unreachable!(),
        }
        self.order.swap_remove(route_index);
    }

    pub(super) fn commit_retain(&mut self, old_to_new: &[Option<usize>], routes: &[RouteEntry]) {
        debug_assert_eq!(old_to_new.len(), self.order.len());
        for (old_index, new_index) in old_to_new.iter().copied().enumerate() {
            if let Some(new_index) = new_index {
                self.order[new_index] = self.order[old_index];
            }
        }
        self.order.truncate(routes.len());
        self.exact.clear();
        self.groups.clear();
        for (route_index, route) in routes.iter().copied().enumerate() {
            self.exact.insert(route, route_index);
            self.restore_member(route_index, route);
        }
    }

    pub(super) fn commit_replace(&mut self, route_index: usize, old: RouteEntry, new: RouteEntry) {
        debug_assert_eq!(ConflictKey::new(old), ConflictKey::new(new));
        self.exact.remove(&old);
        self.exact.insert(new, route_index);
    }

    pub(super) fn remap_moved(&mut self, old_index: usize, new_index: usize, route: RouteEntry) {
        self.exact.insert(route, new_index);
        let group = self.groups.get_mut(&ConflictKey::new(route)).unwrap();
        if group.first == old_index {
            group.first = new_index;
        }
        if group.last == old_index {
            group.last = new_index;
        }
    }

    pub(super) fn order(&self, route_index: usize) -> AliasOrder {
        self.order[route_index]
    }

    pub(super) fn orders(&self) -> &[AliasOrder] {
        &self.order
    }

    fn restore_member(&mut self, route_index: usize, route: RouteEntry) {
        let key = ConflictKey::new(route);
        let order = self.order[route_index];
        match self.groups.get_mut(&key) {
            Some(group) => {
                if order.member < group.head {
                    group.head = order.member;
                    group.first = route_index;
                }
                if order.member > group.tail {
                    group.tail = order.member;
                    group.last = route_index;
                }
            }
            None => {
                self.groups.insert(
                    key,
                    ConflictGroup {
                        order: order.group,
                        head: order.member,
                        tail: order.member,
                        first: route_index,
                        last: route_index,
                    },
                );
            }
        }
    }
}

fn try_clone_map<K, V>(source: &HashMap<K, V>) -> Result<HashMap<K, V>, SystemError>
where
    K: Copy + Eq + core::hash::Hash,
    V: Copy,
{
    let mut cloned = HashMap::new();
    cloned
        .try_reserve(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    cloned.extend(source.iter().map(|(key, value)| (*key, *value)));
    Ok(cloned)
}

fn try_clone_vec<T: Copy>(source: &[T]) -> Result<Vec<T>, SystemError> {
    let mut cloned = Vec::new();
    cloned
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    cloned.extend_from_slice(source);
    Ok(cloned)
}
