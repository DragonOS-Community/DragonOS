//! Linux 6.6 non-recursive memory.min/low budget distribution.
//!
//! This is only the arithmetic step of a top-down reclaim snapshot. The
//! reclaim root ignores its own protection; its immediate children use their
//! settings directly. Deeper children use the result calculated for their
//! parent. Unused sibling settings never consume the parent's budget.

use alloc::{sync::Arc, vec::Vec};
use hashbrown::HashMap;
use system_error::SystemError;

use super::{MemoryAccount, MemoryEvent, MemoryStat};

struct ProtectionEntry {
    account: Arc<MemoryAccount>,
    parent: Option<usize>,
    usage: u64,
    min: u64,
    low: u64,
    children_min: u64,
    children_low: u64,
    effective_min: u64,
    effective_low: u64,
    low_recorded: bool,
    file_lru: u64,
    children_file_lru: u64,
    scan_remaining: u64,
}

/// A single reclaim pass's top-down, non-recursive protection budgets. Account
/// identity, not a reused directory ID, distinguishes historical generations.
pub(crate) struct ProtectionSnapshot {
    accounts: Vec<Arc<MemoryAccount>>,
    entries: Vec<ProtectionEntry>,
    indices: HashMap<usize, usize>,
    low_skipped: bool,
    unowned_remaining: u64,
}

pub(crate) use super::registry::{global_snapshot, try_global_snapshot};

impl ProtectionSnapshot {
    pub fn new(root: &Arc<MemoryAccount>) -> Result<Self, SystemError> {
        let accounts = super::registry::snapshot()?;
        let mut snapshot = Self::with_accounts(accounts)?;
        snapshot.populate(root);
        Ok(snapshot)
    }

    /// Reserve the entire global recovery dependency chain before publication.
    pub(super) fn with_capacity(capacity: usize) -> Result<Self, SystemError> {
        let mut accounts = Vec::new();
        accounts
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        Self::with_accounts(accounts)
    }

    fn with_accounts(accounts: Vec<Arc<MemoryAccount>>) -> Result<Self, SystemError> {
        let mut entries = Vec::new();
        let mut indices = HashMap::new();
        entries
            .try_reserve_exact(accounts.capacity())
            .map_err(|_| SystemError::ENOMEM)?;
        indices
            .try_reserve(accounts.capacity())
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self {
            accounts,
            entries,
            indices,
            low_skipped: false,
            unowned_remaining: 0,
        })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.entries.is_empty() && self.indices.is_empty()
    }

    pub(super) fn capacity(&self) -> usize {
        self.accounts
            .capacity()
            .min(self.entries.capacity())
            .min(self.indices.capacity())
    }

    /// Called under the registry lock, only after checking the complete stock.
    pub(super) fn push_account(&mut self, account: Arc<MemoryAccount>) {
        assert!(self.accounts.len() < self.capacity());
        self.accounts.push(account);
    }

    /// Retain backing storage, but never pin an old controller generation.
    pub(super) fn clear(&mut self) {
        self.accounts.clear();
        self.entries.clear();
        self.indices.clear();
        self.low_skipped = false;
        self.unowned_remaining = 0;
    }

    pub(super) fn populate(&mut self, root: &Arc<MemoryAccount>) {
        assert!(self.entries.is_empty() && self.indices.is_empty());
        assert!(self.accounts.len() <= self.capacity());
        let entries = &mut self.entries;
        let indices = &mut self.indices;
        // Registry order is parent-before-child. Thus testing membership needs
        // one parent lookup, not an O(depth) walk for every inventory entry.
        for account in self.accounts.drain(..) {
            let parent = if Arc::ptr_eq(&account, root) {
                None
            } else {
                let Some(parent) = account
                    .parent()
                    .and_then(|parent| indices.get(&(Arc::as_ptr(parent) as usize)).copied())
                else {
                    continue;
                };
                Some(parent)
            };
            let usage = account.current_pages();
            let policy = account.policy();
            let (min, low) = if account.is_online() {
                (policy.min, policy.low)
            } else {
                (0, 0)
            };
            if let Some(parent) = parent {
                // Concurrent usage snapshots are approximate. Saturation keeps
                // transiently inconsistent sibling sums conservative and safe.
                entries[parent].children_min =
                    entries[parent].children_min.saturating_add(usage.min(min));
                entries[parent].children_low =
                    entries[parent].children_low.saturating_add(usage.min(low));
            }
            indices.insert(Arc::as_ptr(&account) as usize, entries.len());
            entries.push(ProtectionEntry {
                account,
                parent,
                usage,
                min,
                low,
                children_min: 0,
                children_low: 0,
                effective_min: 0,
                effective_low: 0,
                low_recorded: false,
                file_lru: 0,
                children_file_lru: 0,
                scan_remaining: 0,
            });
        }
        for index in 0..entries.len() {
            let Some(parent) = entries[index].parent else {
                continue;
            };
            let (min, low) = if entries[parent].parent.is_none() {
                // First-level values are the declared settings, not a share
                // of root protection (which is ignored for this reclaim).
                (entries[index].min, entries[index].low)
            } else {
                (
                    effective_protection(
                        entries[index].usage,
                        entries[index].min,
                        entries[parent].effective_min,
                        entries[parent].children_min,
                    ),
                    effective_protection(
                        entries[index].usage,
                        entries[index].low,
                        entries[parent].effective_low,
                        entries[parent].children_low,
                    ),
                )
            };
            entries[index].effective_min = min;
            entries[index].effective_low = low;
        }
    }

    /// Once per priority, outside all page/LRU locks. Hierarchical vmstats
    /// minus immediate children approximate this account's local file LRU;
    /// total usage (including kernel objects) only determines protection.
    pub fn prepare_scan(&mut self, priority: u32, low_reclaim: bool, unowned_lru: usize) {
        for entry in &mut self.entries {
            entry.usage = entry.account.current_pages();
            let policy = entry.account.policy();
            (entry.min, entry.low) = if entry.account.is_online() {
                (policy.min, policy.low)
            } else {
                (0, 0)
            };
            let stats = entry.account.snapshot().stats;
            entry.file_lru = stats[MemoryStat::InactiveFile as usize]
                .saturating_add(stats[MemoryStat::ActiveFile as usize]);
            entry.children_file_lru = 0;
            entry.children_min = 0;
            entry.children_low = 0;
            entry.effective_min = 0;
            entry.effective_low = 0;
        }
        for index in 0..self.entries.len() {
            let entry = &self.entries[index];
            let Some(parent) = entry.parent else {
                continue;
            };
            let (min, low, lru) = (
                entry.usage.min(entry.min),
                entry.usage.min(entry.low),
                entry.file_lru,
            );
            let parent = &mut self.entries[parent];
            parent.children_min = parent.children_min.saturating_add(min);
            parent.children_low = parent.children_low.saturating_add(low);
            parent.children_file_lru = parent.children_file_lru.saturating_add(lru);
        }
        for index in 0..self.entries.len() {
            if let Some(parent) = self.entries[index].parent {
                let (min, low) = if self.entries[parent].parent.is_none() {
                    (self.entries[index].min, self.entries[index].low)
                } else {
                    (
                        effective_protection(
                            self.entries[index].usage,
                            self.entries[index].min,
                            self.entries[parent].effective_min,
                            self.entries[parent].children_min,
                        ),
                        effective_protection(
                            self.entries[index].usage,
                            self.entries[index].low,
                            self.entries[parent].effective_low,
                            self.entries[parent].children_low,
                        ),
                    )
                };
                self.entries[index].effective_min = min;
                self.entries[index].effective_low = low;
            }
            let entry = &mut self.entries[index];
            let local_lru = entry.file_lru.saturating_sub(entry.children_file_lru);
            let low_scaled = !low_reclaim && entry.effective_low > entry.effective_min;
            if low_scaled && local_lru != 0 && entry.usage > entry.effective_min {
                self.low_skipped = true;
            }
            let protection = if low_scaled {
                entry.effective_low
            } else {
                entry.effective_min
            };
            let eligible = entry.parent.is_none()
                || !entry.account.is_online()
                || (entry.usage > entry.effective_min
                    && (low_reclaim || entry.usage > entry.effective_low));
            entry.scan_remaining = if eligible {
                scan_budget(
                    local_lru,
                    entry.usage,
                    protection,
                    priority,
                    entry.account.is_online(),
                )
            } else {
                0
            };
        }
        self.unowned_remaining = (unowned_lru as u64) >> priority;
    }

    pub fn has_scan_budget(&self) -> bool {
        self.unowned_remaining != 0 || self.entries.iter().any(|entry| entry.scan_remaining != 0)
    }

    /// Consume only once at the initial scan, never again at final unlink.
    pub fn take_scan(&mut self, account: Option<&Arc<MemoryAccount>>, low_reclaim: bool) -> bool {
        let Some(account) = account else {
            if self.unowned_remaining == 0 {
                return false;
            }
            self.unowned_remaining -= 1;
            return true;
        };
        let Some(index) = self.indices.get(&(Arc::as_ptr(account) as usize)).copied() else {
            return false;
        };
        if self.entries[index].scan_remaining == 0 || !self.permits(account, low_reclaim) {
            return false;
        }
        self.entries[index].scan_remaining -= 1;
        true
    }

    /// A new generation not seen in this pass is conservatively skipped; the
    /// next pass rebuilds budgets. The target's own protection never blocks it.
    pub fn permits(&mut self, account: &Arc<MemoryAccount>, low_reclaim: bool) -> bool {
        let Some(index) = self.indices.get(&(Arc::as_ptr(account) as usize)).copied() else {
            return false;
        };
        let entry = &mut self.entries[index];
        if entry.parent.is_none() {
            return true;
        }
        // Offline may happen after the snapshot. It removes protection, not
        // page ownership or reclaim-scope identity.
        if !entry.account.is_online() {
            return true;
        }
        let usage = entry.account.current_pages();
        if usage <= entry.effective_min {
            return false;
        }
        if usage <= entry.effective_low {
            if !low_reclaim {
                self.low_skipped = true;
                return false;
            }
            if !entry.low_recorded {
                entry.account.record_event(MemoryEvent::Low);
                entry.low_recorded = true;
            }
        }
        true
    }

    pub fn low_skipped(&self) -> bool {
        self.low_skipped
    }
}

/// Linux get_scan_count's raw file scan target, including the protected 32
/// page floor before priority quantization. This is not a reclaim/free goal.
pub(crate) fn scan_budget(
    lru: u64,
    usage: u64,
    protection: u64,
    priority: u32,
    online: bool,
) -> u64 {
    if lru == 0 {
        return 0;
    }
    let scan = if protection == 0 {
        lru
    } else {
        let protected = (lru as u128 * protection as u128) / (usage.max(protection) as u128 + 1);
        (lru - protected as u64).max(32)
    } >> priority;
    if scan == 0 && !online {
        lru.min(32)
    } else {
        scan
    }
}

pub(crate) fn effective_protection(
    usage: u64,
    setting: u64,
    parent_effective: u64,
    siblings_protected: u64,
) -> u64 {
    let protected = usage.min(setting);
    if siblings_protected > parent_effective {
        // The caller includes this child's utilized protection in the sum.
        // Multiplication needs a wider intermediate even for valid counters.
        ((protected as u128 * parent_effective as u128) / siblings_protected as u128) as u64
    } else {
        protected
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_protection, scan_budget, ProtectionSnapshot};
    use crate::cgroup::memory::{MemoryAccount, MemoryEvent, MemoryKind, MemoryPolicy, MemoryStat};

    #[test]
    fn raw_scan_matches_linux_quantization_and_total_usage() {
        assert_eq!(scan_budget(4096, 5000, 3000, 0, true), 1639);
        assert_eq!(scan_budget(4096, 5000, 1000, 0, true), 3277);
        assert_eq!(scan_budget(4096, 5000, 3000, 12, true), 0);
        assert_eq!(scan_budget(8, 8, 8, 0, true), 32);
        assert_eq!(scan_budget(8, 8, 8, 6, true), 0);
        assert_eq!(scan_budget(8, 8, 0, 12, false), 8);
        assert_eq!(scan_budget(0, 100, 50, 0, true), 0);
        assert_eq!(scan_budget(u64::MAX, u64::MAX, u64::MAX, 0, true), 32);
        // Kernel/anonymous usage contributes to the denominator, not LRU size.
        assert!(scan_budget(4096, 6000, 3000, 0, true) > scan_budget(4096, 4096, 3000, 0, true));
    }

    #[test]
    fn per_owner_scan_stock_is_not_reset_by_repeated_candidates() {
        let root = MemoryAccount::new(8001, None).unwrap();
        let parent = MemoryAccount::new(8002, Some(root.clone())).unwrap();
        let a = MemoryAccount::new(8003, Some(parent.clone())).unwrap();
        let b = MemoryAccount::new(8004, Some(parent.clone())).unwrap();
        let _a = a.try_charge(MemoryKind::File, 100).unwrap();
        let _b = b.try_charge(MemoryKind::File, 100).unwrap();
        a.adjust_stat(MemoryStat::InactiveFile, 100, true);
        b.adjust_stat(MemoryStat::InactiveFile, 100, true);
        parent.set_policy(MemoryPolicy {
            min: 80,
            ..Default::default()
        });
        a.set_policy(MemoryPolicy {
            min: 75,
            ..Default::default()
        });
        b.set_policy(MemoryPolicy {
            min: 25,
            ..Default::default()
        });
        let mut snapshot = ProtectionSnapshot::new(&root).unwrap();
        snapshot.prepare_scan(0, false, 0);
        let mut count_a = 0;
        while snapshot.take_scan(Some(&a), false) {
            count_a += 1;
        }
        let mut count_b = 0;
        while snapshot.take_scan(Some(&b), false) {
            count_b += 1;
        }
        assert_eq!(count_a, scan_budget(100, 100, 60, 0, true));
        assert_eq!(count_b, scan_budget(100, 100, 20, 0, true));
        assert!(count_a < count_b);
        assert!(!snapshot.has_scan_budget());
        // Swapping policy, not cache birth order, swaps the scan advantage.
        a.set_policy(MemoryPolicy {
            min: 25,
            ..Default::default()
        });
        b.set_policy(MemoryPolicy {
            min: 75,
            ..Default::default()
        });
        snapshot.prepare_scan(0, false, 0);
        let mut swapped_a = 0;
        while snapshot.take_scan(Some(&a), false) {
            swapped_a += 1;
        }
        let mut swapped_b = 0;
        while snapshot.take_scan(Some(&b), false) {
            swapped_b += 1;
        }
        assert_eq!((swapped_a, swapped_b), (count_b, count_a));
        a.adjust_stat(MemoryStat::InactiveFile, 100, false);
        b.adjust_stat(MemoryStat::InactiveFile, 100, false);
    }

    #[test]
    fn low_scaling_requests_fallback_and_target_has_no_own_protection() {
        let root = MemoryAccount::new(8101, None).unwrap();
        let child = MemoryAccount::new(8102, Some(root.clone())).unwrap();
        let _pages = child.try_charge(MemoryKind::File, 100).unwrap();
        child.adjust_stat(MemoryStat::InactiveFile, 100, true);
        child.set_policy(MemoryPolicy {
            min: 20,
            low: 60,
            ..Default::default()
        });
        let mut snapshot = ProtectionSnapshot::new(&root).unwrap();
        snapshot.prepare_scan(0, false, 0);
        assert!(snapshot.low_skipped());
        let first = snapshot.entries
            [snapshot.indices[&(alloc::sync::Arc::as_ptr(&child) as usize)]]
            .scan_remaining;
        snapshot.prepare_scan(0, true, 0);
        let fallback = snapshot.entries
            [snapshot.indices[&(alloc::sync::Arc::as_ptr(&child) as usize)]]
            .scan_remaining;
        assert!(first < fallback);
        let mut target = ProtectionSnapshot::new(&child).unwrap();
        target.prepare_scan(0, false, 0);
        assert_eq!(target.entries[0].scan_remaining, 100);
        child.adjust_stat(MemoryStat::InactiveFile, 100, false);
    }

    #[test]
    fn fallback_eligibility_requires_an_actual_low_refusal() {
        let root = MemoryAccount::new(1, None).unwrap();
        let min = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let low = MemoryAccount::new(3, Some(root.clone())).unwrap();
        min.set_policy(MemoryPolicy {
            min: 8,
            low: 8,
            ..Default::default()
        });
        low.set_policy(MemoryPolicy {
            low: 8,
            ..Default::default()
        });
        let _min_pages = min.try_charge(MemoryKind::File, 8).unwrap();
        let _low_pages = low.try_charge(MemoryKind::File, 8).unwrap();
        let unrelated = MemoryAccount::new(4, None).unwrap();
        let mut snapshot = ProtectionSnapshot::new(&root).unwrap();
        assert!(!snapshot.permits(&min, false));
        assert!(!snapshot.permits(&unrelated, false));
        assert!(!snapshot.low_skipped());
        assert!(!snapshot.permits(&low, false));
        assert!(snapshot.low_skipped());
        assert_eq!(low.snapshot().events_local[MemoryEvent::Low as usize], 0);
        assert!(snapshot.permits(&low, true));
        assert_eq!(low.snapshot().events_local[MemoryEvent::Low as usize], 1);
    }

    #[test]
    fn snapshot_distributes_overcommit_and_observes_reclaimed_usage() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let a = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let b = MemoryAccount::new(4, Some(parent.clone())).unwrap();
        parent.set_policy(MemoryPolicy {
            min: 16,
            ..Default::default()
        });
        a.set_policy(MemoryPolicy {
            min: 24,
            ..Default::default()
        });
        b.set_policy(MemoryPolicy {
            min: 8,
            ..Default::default()
        });
        let mut a_pages = a.try_charge(MemoryKind::File, 16).unwrap();
        let mut b_pages = b.try_charge(MemoryKind::File, 8).unwrap();
        let mut protection = ProtectionSnapshot::new(&root).unwrap();
        assert!(protection.permits(&a, false));
        assert!(protection.permits(&b, false));
        drop(a_pages.split_off(6).unwrap()); // effective min is 10
        drop(b_pages.split_off(3).unwrap()); // effective min is 5
        assert!(!protection.permits(&a, true));
        assert!(!protection.permits(&b, true));
    }

    #[test]
    fn nonrecursive_snapshot_does_not_protect_unconfigured_descendants() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let child = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        parent.set_policy(MemoryPolicy {
            min: 100,
            ..Default::default()
        });
        let _pages = child.try_charge(MemoryKind::File, 8).unwrap();
        let mut protection = ProtectionSnapshot::new(&root).unwrap();
        assert!(!protection.permits(&parent, true));
        assert!(protection.permits(&child, false));
    }

    #[test]
    fn low_fallback_is_once_per_pass_and_never_breaks_min() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        child.set_policy(MemoryPolicy {
            min: 4,
            low: 8,
            ..Default::default()
        });
        let mut pages = child.try_charge(MemoryKind::File, 8).unwrap();
        let mut protection = ProtectionSnapshot::new(&root).unwrap();
        assert!(!protection.permits(&child, false));
        assert!(protection.permits(&child, true));
        assert!(protection.permits(&child, true));
        assert_eq!(child.snapshot().events_local[MemoryEvent::Low as usize], 1);
        drop(pages.split_off(4).unwrap());
        assert!(!protection.permits(&child, true));
    }

    #[test]
    fn retired_instances_are_not_confused_with_same_id_new_generation() {
        let root = MemoryAccount::new(1, None).unwrap();
        let old = MemoryAccount::new(2, Some(root.clone())).unwrap();
        old.set_policy(MemoryPolicy {
            min: 8,
            ..Default::default()
        });
        let pages = old.try_charge(MemoryKind::File, 8).unwrap();
        old.offline();
        let new = MemoryAccount::new(2, Some(root.clone())).unwrap();
        new.set_policy(MemoryPolicy {
            min: 8,
            ..Default::default()
        });
        let _new_pages = new.try_charge(MemoryKind::File, 8).unwrap();
        let mut protection = ProtectionSnapshot::new(&root).unwrap();
        assert!(protection.permits(&old, false));
        assert!(!protection.permits(&new, true));
        drop(protection);
        let old_weak = alloc::sync::Arc::downgrade(&old);
        drop(old);
        assert!(old_weak.upgrade().is_some());
        drop(pages);
        // Another concurrent registry snapshot may transiently pin the retired
        // account. The charge must be gone even while that reader still owns it.
        if let Some(retained) = old_weak.upgrade() {
            assert_eq!(retained.current_pages(), 0);
            assert!(!retained.is_online());
        }
    }

    #[test]
    fn own_reclaim_ignores_root_protection_and_skips_other_or_new_domains() {
        let root = MemoryAccount::new(1, None).unwrap();
        let a = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let b = MemoryAccount::new(3, Some(root)).unwrap();
        a.set_policy(MemoryPolicy {
            min: 100,
            low: 100,
            ..Default::default()
        });
        let _pages = a.try_charge(MemoryKind::File, 8).unwrap();
        let mut protection = ProtectionSnapshot::new(&a).unwrap();
        let new_child = MemoryAccount::new(4, Some(a.clone())).unwrap();
        assert!(protection.permits(&a, false));
        assert!(!protection.permits(&b, true));
        assert!(!protection.permits(&new_child, true));
    }

    #[test]
    fn overcommit_distributes_by_utilization_not_claims() {
        // C claims 24 but uses 16, D claims/uses 8, E claims nothing.
        let siblings = 16 + 8;
        assert_eq!(effective_protection(16, 24, 16, siblings), 10);
        assert_eq!(effective_protection(16, 8, 16, siblings), 5);
        assert_eq!(effective_protection(16, 0, 16, siblings), 0);
        // Integer truncation does not manufacture the remaining page.
    }

    #[test]
    fn unused_claims_do_not_take_a_share() {
        assert_eq!(effective_protection(0, 500, 16, 8), 0);
        assert_eq!(effective_protection(8, 24, 16, 8), 8);
    }

    #[test]
    fn undercommit_does_not_distribute_unclaimed_allowance() {
        assert_eq!(effective_protection(32, 4, 16, 8), 4);
        assert_eq!(effective_protection(32, 0, 16, 8), 0);
        assert_eq!(effective_protection(4, 32, 16, 8), 4);
    }

    #[test]
    fn zero_parent_and_empty_siblings_are_safe() {
        assert_eq!(effective_protection(8, 8, 0, 8), 0);
        assert_eq!(effective_protection(0, 8, 0, 0), 0);
        assert_eq!(effective_protection(0, 0, 16, 0), 0);
    }

    #[test]
    fn multiplication_does_not_wrap_for_large_counters() {
        assert_eq!(
            effective_protection(u64::MAX, u64::MAX, u64::MAX - 1, u64::MAX),
            u64::MAX - 1
        );
        assert_eq!(
            effective_protection(u64::MAX / 2, u64::MAX, u64::MAX / 2, u64::MAX),
            u64::MAX / 4
        );
    }
}
