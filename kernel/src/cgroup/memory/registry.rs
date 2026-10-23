//! Weak controller inventory and preallocated global reclaim snapshots.
//!
//! Account publication prepares recovery storage, outside charge/free paths.
//! A scan leases that storage without retaining the inventory lock. Registration
//! can install a replacement during a scan, without waiting for filesystem/MM
//! resources held by the scanner.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::libs::resource_wait::{ResourceMutex, ResourceMutexGuard};

use super::{protection::ProtectionSnapshot, MemoryAccount};

struct Registry {
    accounts: Vec<Weak<MemoryAccount>>,
    idle: Option<ProtectionSnapshot>,
}

static ACCOUNTS: ResourceMutex<Registry> = ResourceMutex::new(Registry {
    accounts: Vec::new(),
    idle: None,
});
// Only global scans serialize here. In particular, registration and NOIO
// reclaim never acquire this lock, so no topology/resource -> scan lock edge.
static GLOBAL_SCAN: ResourceMutex<()> = ResourceMutex::new(());

/// The initial root is the uncontended bootstrap exception; other callers run
/// in sleepable topology preparation. Every published account has recovery stock.
pub(super) fn register(account: &Arc<MemoryAccount>) -> Result<(), SystemError> {
    let mut prepared = Vec::new();
    let mut workspace: Option<ProtectionSnapshot> = None;
    loop {
        let mut registry = ResourceMutexGuard::lock(&ACCOUNTS);
        registry
            .accounts
            .retain(|account| account.strong_count() != 0);
        let required = registry
            .accounts
            .len()
            .checked_add(1)
            .ok_or(SystemError::ENOMEM)?;
        let grow_accounts = registry.accounts.capacity() < required;
        let grow_workspace = registry
            .idle
            .as_ref()
            .is_none_or(|idle| idle.capacity() < required);
        if (!grow_accounts || prepared.capacity() >= required)
            && (!grow_workspace
                || workspace
                    .as_ref()
                    .is_some_and(|stock| stock.capacity() >= required))
        {
            if grow_accounts {
                prepared.append(&mut registry.accounts);
                core::mem::swap(&mut registry.accounts, &mut prepared);
            }
            if grow_workspace {
                core::mem::swap(&mut registry.idle, &mut workspace);
            }
            registry.accounts.push(Arc::downgrade(account));
            drop(registry);
            // Replaced buffers are retired outside the inventory lock.
            drop(prepared);
            drop(workspace);
            return Ok(());
        }
        drop(registry);
        let capacity = required
            .checked_next_power_of_two()
            .ok_or(SystemError::ENOMEM)?;
        if grow_accounts && prepared.capacity() < required {
            prepared
                .try_reserve_exact(capacity)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        if grow_workspace
            && workspace
                .as_ref()
                .is_none_or(|stock| stock.capacity() < required)
        {
            workspace = Some(ProtectionSnapshot::with_capacity(capacity)?);
        }
        // A competing publication, checkout, or return can change either stock.
        // Recheck both under the same lock before publishing any new account.
    }
}

/// Independent snapshots remain fallible for scoped/NOIO callers. They must
/// not wait for a global scanner that may need their filesystem/MM resources.
pub(super) fn snapshot() -> Result<Vec<Arc<MemoryAccount>>, SystemError> {
    let mut live = Vec::new();
    loop {
        let mut registry = ResourceMutexGuard::lock(&ACCOUNTS);
        registry
            .accounts
            .retain(|account| account.strong_count() != 0);
        let required = registry.accounts.len();
        if live.capacity() < required {
            drop(registry);
            live.try_reserve(required)
                .map_err(|_| SystemError::ENOMEM)?;
            continue;
        }
        for account in registry.accounts.iter() {
            if let Some(account) = account.upgrade() {
                live.push(account);
            }
        }
        return Ok(live);
    }
}

/// Own the complete storage bundle until the scan ends. The lease keeps no
/// registry guard; its strong references and surplus buffers retire outside it.
pub(crate) struct GlobalSnapshot {
    snapshot: Option<ProtectionSnapshot>,
    _scan: ResourceMutexGuard<'static, ()>,
}

impl GlobalSnapshot {
    pub(crate) fn get_mut(&mut self) -> &mut ProtectionSnapshot {
        self.snapshot.as_mut().unwrap()
    }
}

impl Drop for GlobalSnapshot {
    fn drop(&mut self) {
        let mut snapshot = self.snapshot.take().unwrap();
        snapshot.clear();
        let mut retired = Some(snapshot);
        {
            let mut registry = ResourceMutexGuard::lock(&ACCOUNTS);
            if registry
                .idle
                .as_ref()
                .is_none_or(|idle| idle.capacity() < retired.as_ref().unwrap().capacity())
            {
                core::mem::swap(&mut registry.idle, &mut retired);
            }
        }
        drop(retired);
    }
}

pub(crate) fn global_snapshot(root: &Arc<MemoryAccount>) -> GlobalSnapshot {
    let scan = ResourceMutexGuard::lock(&GLOBAL_SCAN);
    let snapshot = {
        let mut registry = ResourceMutexGuard::lock(&ACCOUNTS);
        let mut snapshot = registry
            .idle
            .take()
            .expect("registered accounts have global reclaim storage");
        assert!(snapshot.is_empty() && snapshot.capacity() >= registry.accounts.len());
        for account in &registry.accounts {
            if let Some(account) = account.upgrade() {
                snapshot.push_account(account);
            }
        }
        snapshot
    };
    let mut lease = GlobalSnapshot {
        snapshot: Some(snapshot),
        _scan: scan,
    };
    lease.get_mut().populate(root);
    lease
}

/// A fault must not wait behind another global scan which may itself need
/// writeback or this task's MM. Storage is still prepared at registration.
pub(crate) fn try_global_snapshot(root: &Arc<MemoryAccount>) -> Option<GlobalSnapshot> {
    let scan = ResourceMutexGuard::try_lock(&GLOBAL_SCAN)?;
    let snapshot = {
        let mut registry = ResourceMutexGuard::try_lock(&ACCOUNTS)?;
        let mut snapshot = registry.idle.take()?;
        assert!(snapshot.is_empty() && snapshot.capacity() >= registry.accounts.len());
        for account in &registry.accounts {
            if let Some(account) = account.upgrade() {
                snapshot.push_account(account);
            }
        }
        snapshot
    };
    let mut lease = GlobalSnapshot {
        snapshot: Some(snapshot),
        _scan: scan,
    };
    lease.get_mut().populate(root);
    Some(lease)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::memory::MemoryKind;

    #[test]
    fn registration_during_global_scan_prepares_the_next_complete_snapshot() {
        let root = MemoryAccount::new(9200, None).unwrap();
        let mut first = global_snapshot(&root);
        let mut accounts = Vec::new();
        let mut charges = Vec::new();
        for id in 9201..9266 {
            let child = MemoryAccount::new(id, Some(root.clone())).unwrap();
            charges.push(child.try_charge(MemoryKind::File, 1).unwrap());
            assert!(!first.get_mut().permits(&child, false));
            accounts.push(child);
        }
        let prepared_capacity = {
            let registry = ResourceMutexGuard::lock(&ACCOUNTS);
            let capacity = registry.idle.as_ref().unwrap().capacity();
            assert!(capacity >= registry.accounts.len());
            capacity
        };
        // Returning the older lease must not discard the larger replacement.
        drop(first);
        {
            let _scan = ResourceMutexGuard::lock(&GLOBAL_SCAN);
            let registry = ResourceMutexGuard::lock(&ACCOUNTS);
            assert!(registry.idle.as_ref().unwrap().capacity() >= prepared_capacity);
        }
        let mut next = global_snapshot(&root);
        for account in &accounts {
            assert!(next.get_mut().permits(account, false));
        }
    }

    #[test]
    fn returned_global_snapshot_clears_controller_storage() {
        let root = MemoryAccount::new(9300, None).unwrap();
        let child = MemoryAccount::new(9301, Some(root.clone())).unwrap();
        let weak = Arc::downgrade(&child);
        let lease = global_snapshot(&root);
        drop(child);
        assert!(weak.upgrade().is_some());
        drop(lease);
        let _scan = ResourceMutexGuard::lock(&GLOBAL_SCAN);
        let registry = ResourceMutexGuard::lock(&ACCOUNTS);
        assert!(registry.idle.as_ref().unwrap().is_empty());
        // Other concurrent scoped snapshots may transiently pin this account;
        // the returned global storage itself must contain no strong owners.
    }
}
