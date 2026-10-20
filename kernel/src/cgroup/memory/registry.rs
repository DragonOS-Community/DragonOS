//! Weak controller-instance inventory for sleepable reclaim snapshots.
//!
//! Registration belongs to topology preparation, never to charge/free. Parents
//! are registered before children. Retired instances remain discoverable while
//! actual resource tokens pin them, without keeping dead generations alive.
//! Metadata allocation here is deliberately not recursively memcg-accounted.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::libs::{mutex::Mutex, resource_wait::ResourceMutexGuard};

use super::MemoryAccount;

static ACCOUNTS: Mutex<Vec<Weak<MemoryAccount>>> = Mutex::new(Vec::new());

/// The initial root is the uncontended bootstrap exception; other callers must
/// be in sleepable topology preparation, outside accounting/IRQ/resource locks.
pub(super) fn register(account: &Arc<MemoryAccount>) -> Result<(), SystemError> {
    let mut prepared = Vec::new();
    loop {
        let mut accounts = ResourceMutexGuard::lock(&ACCOUNTS);
        accounts.retain(|account| account.strong_count() != 0);
        if accounts.len() < accounts.capacity() {
            accounts.push(Arc::downgrade(account));
            return Ok(());
        }
        let required = accounts.len().checked_add(1).ok_or(SystemError::ENOMEM)?;
        if prepared.capacity() >= required {
            // Preserve parent-before-child order without allocating under the
            // inventory lock. The replaced, empty backing is retired below.
            prepared.extend(accounts.drain(..));
            core::mem::swap(&mut *accounts, &mut prepared);
            accounts.push(Arc::downgrade(account));
            drop(accounts);
            drop(prepared);
            return Ok(());
        }
        drop(accounts);
        // Growth must remain amortized: an empty spare reserving only n + 1
        // on every insertion would repeatedly move the entire inventory.
        let capacity = required
            .checked_next_power_of_two()
            .ok_or(SystemError::ENOMEM)?;
        prepared
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
    }
}

/// Sleepable, fallible metadata snapshot. No topology locks are acquired here,
/// and callers release this lock before touching page/rmap/LRU state.
pub(super) fn snapshot() -> Result<Vec<Arc<MemoryAccount>>, SystemError> {
    let mut live = Vec::new();
    loop {
        let mut accounts = ResourceMutexGuard::lock(&ACCOUNTS);
        accounts.retain(|account| account.strong_count() != 0);
        let required = accounts.len();
        if live.capacity() < required {
            drop(accounts);
            // live remains empty until the complete current inventory fits.
            // Reserve is relative to len, not the existing spare capacity.
            live.try_reserve(required)
                .map_err(|_| SystemError::ENOMEM)?;
            continue;
        }
        for account in accounts.iter() {
            if let Some(account) = account.upgrade() {
                live.push(account);
            }
        }
        drop(accounts);
        return Ok(live);
    }
}
